# Security

duolane attaches the owner's Codex login to requests it accepts, so being able
to reach its port must not be enough to use that login.

## Trust boundaries

| Actor                                 | Trust                                            |
| ------------------------------------- | ------------------------------------------------ |
| Claude Code on the same machine       | Trusted client                                   |
| A web page in the owner's browser     | Untrusted, and able to send requests to loopback |
| Another local process                 | Limited: blocked by the token when one is set    |
| A host on the network                 | Untrusted; reaches duolane only on a chosen bind |
| Anthropic and OpenAI endpoints        | Trusted for auth, not for response size or shape |
| Model output and tool results         | Untrusted text                                   |

## Inbound requests

1. The default bind address is `127.0.0.1`.
2. duolane refuses to start on a non-loopback address without an inbound token.
3. On a loopback bind, the `Host` header must name a loopback host. Other hosts
   need explicit configuration. This blocks DNS rebinding.
4. A request with an `Origin` header is rejected unless that origin is explicitly
   allowed. This blocks browser cross-site requests.
5. JSON endpoints require a JSON content type, so a browser cannot send them as
   a simple request.
6. These checks run before body parsing, on every route except `/healthz`.
7. When a token is configured, every route except `/healthz` requires it,
   including `/monitor`. `/monitor` also stays limited to loopback peers.
8. Token comparison does not reveal where a mismatch occurred. The inbound token
   is removed before a request is forwarded upstream.

## Credentials

- The Claude login passes through in transit and is never stored or logged.
- The Codex login is read from the Codex CLI's file and is never deleted, even
  after a rejected refresh.
- Writes to that file are atomic, durable, private to the user, keep fields
  duolane does not own, and hold a lock shared across processes. A refresh
  re-reads the file under that lock before spending a refresh token.
- Credential-bearing clients do not follow redirects and use HTTPS. Plain HTTP is
  allowed only for loopback, for tests and local development.
- Connection reuse and continuation state are bound to the account that created
  them. An account change is a cache miss.

## Limits

- Request bodies, upstream bodies, stream buffers, and per-session state each
  have a byte or entry limit.
- Text kept for the monitor or for logs is truncated on a character boundary.
- Error captures and rotated logs have limits on file size, file count, and age.
- Compressed upstream data has a limit on its decompressed size.

## Stored data

- Traffic capture is off by default and warns when enabled, because it stores
  conversation content.
- Logs and captures are private to the user, and redact credentials by key name
  whatever the value's type.
- Query strings are redacted by the same rules as headers and bodies.

## Terminal output

Text from requests, models, and upstream errors has control characters removed
before it reaches the terminal.
