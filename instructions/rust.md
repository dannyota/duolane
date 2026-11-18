# Rust

## Checks

- `make check` runs every offline check and `make help` lists the targets. Run `make deny` and `make crate-age` when `Cargo.lock` changes.
- `make test` uses cargo-nextest, one process per test, with an empty home under `target/`, and fails if a test writes there.
- CI also proves each action SHA is its tag with `scripts/check-workflow-pins.py --tags`, which needs the network.

## Code rules

- Follow the surrounding module. Split files by concern, not by line count, and move code unchanged when splitting.
- Write no comments except the [standard's exceptions](../docs/standards/engineering.md#code-comments). `clap` help text uses `about` and `help` attributes, not doc comments.
- Never slice text by a byte offset unless the offset is a checked character boundary.
- Reachable request or upstream data must not panic. Avoid `unwrap` and `expect` outside tests and startup.
- Every buffer, map, and file set fed by requests or upstream data has a bound in the [security design](../docs/design/security.md#limits).
- `reqwest` stays built with rustls and without automatic decompression, so bodies pass through unchanged.
- Blocking file, process, or network calls do not run on async workers.
