# Release publishing

The tag-triggered Release workflow tests `duolane`, builds six platform
archives, and uploads each archive with its checksum. Publishing to crates.io is
a separate owner decision because an uploaded version cannot be replaced.

## Approaches

Use a separate manual workflow, as in `elasticctl`, with a preflight-only mode.
Publishing automatically from a tag would remove the owner's publish decision.
Publishing from the existing Release job would give one job both GitHub write
access and crates.io access.

## Release gate

- Dispatch `publish-crates.yml` from `main` with a `vMAJOR.MINOR.PATCH` tag. The
  default is preflight only. Both modes share one concurrency group.
- Preflight resolves the tag to a commit in the dispatch commit's history. The
  tagged manifest must name `duolane` and match the tag version. The tag and
  commit must have a successful `release.yml` tag-push run. The published GitHub
  Release must contain all six uploaded archives and their checksums. Each asset
  digest must match the corresponding artifact file from that run. The `duolane`
  crate must list `dannyota` as an owner.
- With publishing requested, preflight also requires an immutable GitHub Release
  and HTTP 404 for the exact version on crates.io. After recording the verified
  outputs, its last step builds the package with
  `cargo publish --dry-run --locked --registry crates-io` at the tagged commit,
  without OIDC or a publish token.
- The publish job requires a fresh dispatch with the explicit publish input and
  `crates-io` environment approval. Before token exchange it checks the owner
  reviewer is still configured and that the run recorded an approval from that
  owner for the environment. It stops if the tag moved or the checkout includes
  a Cargo config. It invokes the pinned numeric Rust version explicitly with an
  empty Cargo home and the built-in `cargo:token` provider. It exchanges GitHub
  OIDC for a short-lived token, then runs
  `cargo publish --no-verify --locked --registry crates-io` without a build
  cache or compilation.

## One-time setup

In GitHub, enable immutable releases before creating the next release. Restrict
the `crates-io` environment to branch `main` and require owner review. If the
dispatcher is the only reviewer, allow self-review. In crates.io, add a [Trusted
Publisher] for `duolane` with repository `dannyota/duolane`, workflow
`publish-crates.yml`, and environment `crates-io`. Do not add a crates.io token
to GitHub secrets.

[Trusted Publisher]: https://crates.io/docs/trusted-publishing
