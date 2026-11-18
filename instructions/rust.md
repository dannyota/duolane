# Rust

## Checks

- `make check` runs every offline check: `lengths`, `comments`, `pins`, `docs`, `fmt`, `lint`, and `test`. `make help` lists every target.
- `make fmt` checks formatting and `make lint` runs clippy with warnings denied.
- `make test` runs `cargo nextest run --locked` with `HOME` and the XDG directories at an empty directory under `target/`, and fails if a test writes there. nextest runs each test in its own process, so tests that mutate process-wide environment variables cannot race. Without cargo-nextest it falls back to `cargo test --locked -- --test-threads=1`.
- `make pins` checks that every workflow action is pinned by full SHA with a version comment and that `.tool-versions`, `rust-toolchain.toml`, `package.json`, and `package-lock.json` agree. CI also runs `scripts/check-workflow-pins.py --tags`, which needs the network, to prove each SHA is its tag.
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
