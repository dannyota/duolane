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
2. Any configured inbound token has at least 32 characters. duolane refuses to
   start on a non-loopback address without one. Inbound traffic is plain HTTP,
   including the token and the Claude login, so such a bind is supported only on
   an encrypted path such as an SSH tunnel, WireGuard, or a TLS-terminating
   proxy. duolane prints this warning at startup.
3. On a loopback bind, the `Host` header must be present once and its host must
   exactly match `localhost`, `127.0.0.1`, `[::1]`, or the literal bind address,
   ignoring case. Any port is allowed. Prefix and suffix matches are forbidden.
4. Additional exact hosts can be configured for a local tunnel or reverse proxy.
   Configuring one requires an inbound token.
5. Any request with an `Origin` header is rejected. duolane sends no CORS
   headers and answers no preflight.
6. `POST` routes that take JSON require the media type `application/json`, and
   reject a missing type. Parameters such as `charset` are allowed.
   `/v1/audio/transcriptions` accepts only `multipart/form-data`, and
   `/v1/images/edits` accepts either type. Multipart is a type browsers can send
   cross-site, so rule 5 is its defense. `GET` and `HEAD` are exempt.
7. When a token is set, every route except `/healthz` requires it. The token
   travels in the `x-duolane-key` header or as a bearer token. `duolane monitor`
   sends the configured token.
8. `/monitor` also accepts only loopback peers. Remote monitoring is
   unsupported.
9. Token comparison reveals neither the token length nor the position of a
   mismatch. The inbound token is removed before a request goes upstream.
10. These checks run before the body is read, on every route except `/healthz`.
    `/healthz` returns no account or request data.

## Credentials

- The Claude login passes through in transit and is never stored or logged.
- The Codex login is read from `$CODEX_HOME/auth.json`, by default
  `~/.codex/auth.json`, and is never deleted, even after a rejected refresh.
- Writes to that file are atomic, flushed to disk, readable only by the user,
  and keep every field duolane does not own.
- duolane processes serialize a refresh with a file lock that has a 10 second
  timeout. Under the lock, duolane re-reads the file before spending a refresh
  token.
- The Codex CLI does not take this lock, so both can still try the same refresh
  token. After a rejected refresh, duolane re-reads the file and uses a token
  that changed. Otherwise it reports that a new `codex login` is needed.
- Connection reuse and continuation state are bound to the account that created
  them. An account change is a cache miss.

## Upstream connections

- Every client that sends a credential disables redirects and sets a connect
  timeout.
- Upstream URLs use HTTPS. Plain HTTP is allowed only for a loopback host, for
  tests and local development.
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
| Open inbound connections            | 256                       |
| Buffered upstream response          | 64 MiB                    |
| Image response                      | 128 MiB                   |
| Transcription response              | 1 MiB                     |
| One upstream SSE or WebSocket frame | 8 MiB                     |
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

A streamed Claude response has no total size limit, because duolane forwards it
without buffering. The limit on idle time between bytes still applies. Text is
truncated on a character boundary.

## Stored data

- Traffic capture is off by default and warns when enabled, because it stores
  conversation content.
- Logs and captures are readable only by the user.
- Credentials are redacted by key name whatever the value's type. Header names,
  body keys, and query parameters share one matcher, which also covers names
  ending in `token`, `secret`, or `password`.

## Terminal output

Text from requests, models, and upstream errors has control characters removed
before it reaches the terminal.
