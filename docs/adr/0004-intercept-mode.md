# ADR 0004: Intercept mode keeps Claude Code on its first-party endpoint

Status: accepted 2026-11-19 by the owner. The mode is not built yet.

## Context

Claude Code turns off Remote Control whenever `ANTHROPIC_BASE_URL` points at a
host other than `api.anthropic.com`
([Remote Control](https://code.claude.com/docs/en/remote-control)). duolane's
gateway mode needs that variable, so a session cannot have both Codex models and
Remote Control.

Claude Code honors `HTTPS_PROXY` and trusts an extra CA file named by
`NODE_EXTRA_CA_CERTS`
([network configuration](https://code.claude.com/docs/en/network-config)). The
owner asked for a mode that inspects TLS, with automatic setup, a cleanup
command, and trust limited to the Claude Code API host.

## Decision

- duolane gains an intercept mode beside the gateway mode. Claude Code leaves
  `ANTHROPIC_BASE_URL` unset and reaches duolane as an HTTP proxy.
- duolane terminates TLS only for `api.anthropic.com`. It tunnels every other
  host without reading it.
- Trust is per process: Claude Code and the programs it starts trust duolane's
  certificate, through `NODE_EXTRA_CA_CERTS`. duolane installs nothing in an
  operating system trust store and needs no `sudo`.
- The certificate authority is limited to `api.anthropic.com` by a critical name
  constraint, and its private key is discarded once it has signed one leaf.
- `duolane claude` starts Claude Code in this mode and changes nothing
  persistent. `duolane intercept setup` writes the same settings for background
  agents, and `duolane intercept cleanup` finds and removes everything setup
  created.
- The [intercept design](../design/intercept.md) owns the rules.

## Consequences

- A session can use Codex models and keep Remote Control, if the unverified
  behavior in the design holds. A spike checks it before the mode is built.
- duolane holds a key that can impersonate `api.anthropic.com` to any process
  that trusts its certificate, so that file is as sensitive as a login.
- Gateway mode stays for clients that cannot use a proxy.

## Alternatives rejected

- Installing the certificate in the system or user trust store. Other
  applications on the machine would trust it too, and it would need removing
  from that store later.
- A certificate authority without a name constraint. Its key could impersonate
  any site.
- Redirecting `api.anthropic.com` with `/etc/hosts` or packet filter rules. It
  changes the whole machine and needs `sudo`.

## History

- 2026-11-19: accepted. The owner first asked for `sudo` setup, then chose
  per-process trust after seeing that Claude Code supports it. Design review
  made the wrapper command the default and settings changes opt-in.
