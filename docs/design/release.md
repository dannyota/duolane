# Release publishing

The tag-triggered Release workflow checks `duolane`, builds archives for macOS
on Apple silicon, Linux on x86-64 and Arm64, and Windows on x86-64, and uploads
each archive with its checksum. Other platforms build from source with Cargo.
Publishing to crates.io is a separate owner decision because an uploaded version
cannot be replaced.

## Checked commits

The Release workflow reuses the tests that CI already passed on the tagged
commit, so a release does not wait for the same tests a third time.

CI still runs every job for a push to `master`. That run blocks nothing, because
the pull request already merged, and proving that a pull request run tested
exactly the merged tree needs more history than GitHub records reliably.

### Proof at the tag

The tests of the Release workflow are skipped only when all of these hold:

- A completed, successful CI run for a `push` event exists in this repository at
  the commit the Release workflow checked out. A push run checks out the pushed
  commit with the workflow file of that commit, so it tested that tree whatever
  its ref.
- In the latest attempt of that run, the job `rust` completed with success.
- The commit is `master` or an ancestor of it, with `master` resolved from
  `refs/heads/master` just before the proof is accepted.

The workflow waits up to five minutes for a matching run that is still in
progress. Without the proof it runs the tests itself, as before.

The dependency policy always runs at the tag, because an advisory can appear
after the pull request. The builds start only after the dependency policy passed
and either the proof was found or the tests passed.

### Lookups

The job that looks for proof never fails the run: each of its steps continues on
error, and a missing or unexpected answer means no proof. It reads GitHub's API
with a read-only token, writes nothing, and passes no text from a pull request
or ref into a shell command.

## Approaches

Use a separate manual workflow, as in `elasticctl`, with a preflight-only mode.
Publishing automatically from a tag would remove the owner's publish decision.
Publishing from the existing Release job would give one job both GitHub write
access and crates.io access.

## Release gate

- Dispatch `publish-crates.yml` from `master` with a `vMAJOR.MINOR.PATCH` tag.
  The default is preflight only. Both modes share one concurrency group and
  require the exact branch name. Publishing additionally requires protected
  `master`.
- Preflight resolves the tag to a commit in the dispatch commit's history. The
  tagged manifest must describe the standalone `duolane` crate, match the tag
  version, and use tracked regular files for explicit README and license paths.
  The tag and commit must have a successful `release.yml` tag-push run. The
  published GitHub Release must contain all four uploaded archives and their
  checksums. Each asset digest must match the corresponding artifact file from
  that run. The `duolane` crate must list `dannyota` as an owner.
- With publishing requested, preflight also requires an immutable GitHub Release
  and HTTP 404 for the exact version on crates.io. After recording the verified
  outputs, its last step builds the package with
  `cargo publish --dry-run --locked --registry crates-io` at the tagged commit,
  without OIDC or a publish token.
- The publish job requires a fresh dispatch with the explicit publish input and
  `crates-io` environment approval. Before token exchange it checks the owner
  reviewer is still configured and that the run recorded an approval from that
  owner for the environment. It stops if the tag moved or the checkout includes
  a Cargo config or a tracked symlink. It invokes the pinned numeric Rust
  version explicitly with an empty Cargo home and the built-in `cargo:token`
  provider. It exchanges GitHub OIDC for a short-lived token, then runs
  `cargo publish --no-verify --locked --registry crates-io` without a build
  cache or compilation.

## One-time setup

In GitHub, enable immutable releases before creating the next release. Restrict
the `crates-io` environment to branch `master` and require owner review. If the
dispatcher is the only reviewer, allow self-review. In crates.io, add a [Trusted
Publisher] for `duolane` with repository `dannyota/duolane`, workflow
`publish-crates.yml`, and environment `crates-io`. Do not add a crates.io token
to GitHub secrets.

[Trusted Publisher]: https://crates.io/docs/trusted-publishing
