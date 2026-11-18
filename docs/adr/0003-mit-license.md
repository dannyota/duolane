# ADR 0003: MIT license with inherited copyright retained

Status: accepted 2026-11-18 by the owner.

## Context

The inherited code is under the MIT License, which requires its copyright and
permission notice to stay with the code. The owner does not need the patent
grant that Apache-2.0 adds.

## Decision

duolane stays under the MIT License. `LICENSE` keeps the inherited copyright
line and adds the owner's.

## Consequences

- One license covers the whole repository, with no separate notice file.
- `LICENSE` ships with source and binary releases.

## Alternatives rejected

- Apache-2.0. It adds an explicit patent grant the owner does not need, and
  would require keeping the inherited MIT notice in a second file.

## History

- 2026-11-18: accepted.
