# Contributing

Bug reports and focused fixes are welcome. Read the
[architecture](docs/design/architecture.md) and
[security design](docs/design/security.md) first.

## Making a change

1. Write a failing test. Tests use mock upstreams and toy credentials, never a
   real account.
2. Make the smallest change that passes it.
3. Update the design or README if behavior changes.
4. Run the checks:

   ```bash
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test -- --test-threads=1
   cargo deny check advisories
   ```

Code has no comments, as the
[engineering standard](docs/standards/engineering.md#code-comments) explains.
Contributions are licensed under the MIT License.
