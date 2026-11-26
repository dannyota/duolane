# Release publishing

The tag-triggered Release workflow checks `duolane`, builds archives for macOS
on Apple silicon, Linux on x86-64 and Arm64, and Windows on x86-64, and uploads
each archive with its checksum. Other platforms build from source with Cargo.
Publishing to crates.io is a separate owner decision because an uploaded version
cannot be replaced.

## Checked commits

The Release workflow reuses the tests that CI already passed on the tagged
commit, so a release does not wait for the same tests a third time.

CI runs every job for a push to `master`, except the `rust` job under the rule
below. That run blocks nothing, because the pull request already merged, and
proving that a pull request run tested exactly the merged tree needs more
history than GitHub records reliably.

### Changes that cannot affect the tests

CI skips the `rust` job for a pull request or a push to `master` when the change
cannot alter what it tests, and the parent already passed it. Every other job
still runs, including the dependency policy, so a version bump still checks
`Cargo.lock`. Scheduled and manual runs always test.

- The change is the diff from the first parent to the checked-out commit: for a
  pull request, the merge commit GitHub builds, whose first parent is the base;
  for a push, the pushed commit.
- Every changed path, on both sides of a rename or deletion, is a regular,
  non-executable file in both trees where it exists, and is either a Markdown
  document (`docs/**.md`, `instructions/**.md`, or `AGENTS.md`, `CHANGELOG.md`,
  `CLAUDE.md`, `CONTRIBUTING.md`, `README.md`, `SECURITY.md` at the root), or
  `Cargo.toml` or `Cargo.lock`. Symlinks, submodules, and any other path make
  `rust` run. No build script, test, or code reads these documents.
- A version change is accepted only when, in each tree, the `duolane` version is
  valid, `Cargo.toml` and the single `duolane` entry of `Cargo.lock` without a
  `source` agree, and both files parse equal to the parent's once that version
  is set to the parent's. The version reaches the binary only through
  `CARGO_PKG_VERSION`, in `--version` and the Codex `User-Agent`, which tests
  read from the same variable.
- The first parent has a completed, successful CI run for a `push` event in this
  repository whose latest attempt passed `ci`, waiting up to five minutes for
  one in progress. A push of several commits needs that run for the pushed
  commit's own first parent, not for the previous `master` head. `ci` passes
  only when `rust` passed or was skipped by this rule, so each skip rests on a
  tested ancestor.
- The deciding job runs the classifier as it is in the first parent, not as the
  change edits it, with read-only `contents` and `actions` permissions. A pull
  request that edits the workflow itself can still change what runs, as it can
  today; the owner reviews every pull request before it merges.

`ci` accepts a skipped `rust` only when the job that decided reported the skip;
in every other case `rust` must succeed. The proof at the tag accepts a push run
whose `ci` passed, for the same reason.

### Proof at the tag

The tests of the Release workflow are skipped only when all of these hold:

- A completed, successful CI run for a `push` event exists in this repository at
  the commit the Release workflow checked out. A push run checks out the pushed
  commit with the workflow file of that commit, so it tested that tree, or
  proved by the rule above that its tests match a tested ancestor's, whatever
  its ref.
- In the latest attempt of that run, the job `ci` completed with success.
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
