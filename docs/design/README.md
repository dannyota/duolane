# Design

These documents describe the intended product. Where the code differs, the
[plans](../plans/) track the gap.

| Design                                | Owns                                                   |
| ------------------------------------- | ------------------------------------------------------ |
| [Architecture](architecture.md)       | Routing, request flow, and invariants                  |
| [Codex route](codex.md)               | Translation, lanes, retries, and Codex errors          |
| [Continuation](continuation.md)       | When a Codex request may send only its new items       |
| [Security](security.md)               | Trust boundaries, inbound rules, credentials, limits   |
| [Intercept mode](intercept.md)        | TLS interception for the Claude Code API host          |
| [Claude accounts](claude-accounts.md) | Named Claude Code login selection and local history    |
| [OpenAI API key](openai-api-key.md)   | Codex credential sources, their order, and key storage |
| [Monitor controls](monitor.md)        | Dashboard input and terminal ownership                 |
| [Release publishing](release.md)      | GitHub Release and crates.io publishing gates          |
