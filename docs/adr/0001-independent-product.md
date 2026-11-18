# ADR 0001: duolane is an independent product

Status: accepted 2026-11-18 by the owner.

## Context

This code began as `raine/claude-code-proxy` and its fork
`fcakyon/claude-code-with-codex`. The owner wants a product of their own, built
and documented like their other repositories. Removing code comments,
restructuring files, and renaming the product would make every upstream merge a
large conflict. The inherited name also contains two vendor trademarks, and the
`claude-codex` crate on crates.io belongs to another owner.

## Decision

- The product, command, crate, and repository are named `duolane`.
- `dannyota/duolane` is a standalone public repository, not a GitHub fork. It
  keeps the inherited commit history, which preserves authorship.
- Upstream changes are ported selectively, by reading them and applying what
  fits this design. They are not merged.
- No compatibility is kept with inherited names, environment variables, or file
  locations, because this product has no installed users.
- Releases start at a new version line. Inherited tags are not published.

## Consequences

- Upstream fixes need deliberate review. The remotes `upstream` and `fcakyon`
  stay available for that purpose.
- Style and structure can change freely.
- The header sent to the ChatGPT backend changes with the product name, so a
  live check must confirm it before the first release.

## Alternatives rejected

- Keeping a GitHub fork. Pull requests default toward the parent, and the
  repository would stay tied to its network.
- Regular upstream merges. They conflict with removing comments and splitting
  files.
- A fresh history. It would discard authorship for no technical gain.
- `claude-openai`. It reads like an official product and uses two trademarks.

## History

- 2026-11-18: accepted.
