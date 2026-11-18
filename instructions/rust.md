# Rust

## Checks

- `make check` runs every offline check: `lengths`, `comments`, `pins`, `docs`, `fmt`, `lint`, and `test`. `make help` lists every target.
- `make fmt` checks formatting and `make lint` runs clippy with warnings denied.
- `make test` runs `cargo test --locked -- --test-threads=1` with `HOME` and the XDG directories at an empty directory under `target/`, and fails if a test writes there. Config tests mutate process-wide environment variables and race under the parallel runner.
- `make deny` runs `cargo deny --locked check`, which blocks merge.
- `make crate-age` checks the crate cool-down against `origin/main`, or `BASE_REF` when set.

## Code rules

- Follow the surrounding module. Split files by concern, not by line count, and move code unchanged when splitting.
- Write no comments except the [standard's exceptions](../docs/standards/engineering.md#code-comments). `clap` help text uses `about` and `help` attributes, not doc comments.
- Never slice text by a byte offset unless the offset is a checked character boundary.
- Reachable request or upstream data must not panic. Avoid `unwrap` and `expect` outside tests and startup.
- Every buffer, map, and file set fed by requests or upstream data has a bound in the [security design](../docs/design/security.md#limits).
- `reqwest` stays built with rustls and without automatic decompression, so bodies pass through unchanged.
- Blocking file, process, or network calls do not run on async workers.
