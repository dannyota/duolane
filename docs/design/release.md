# Release publishing

The tag-triggered Release workflow checks `duolane`, builds archives for macOS
on Apple silicon, Linux on x86-64 and Arm64, and Windows on x86-64, and uploads
each archive with its checksum. Other platforms build from source with Cargo.
Publishing to crates.io is a separate owner decision because an uploaded version
cannot be replaced.

## Checked commits

CI and the Release workflow reuse passing tests when the conditions below hold.
The checks that depend on history or on the day they run still run each time.

### Push to master

A CI run for a push to `refs/heads/master` skips only the `rust` job, and only
with proof that this repository's own checks already passed on the pushed tree.
All of these must hold:

- The pushed commit is the merge commit of exactly one merged pull request of
  this repository into `master`, and its second parent is the pull request's
  head.
- The head already contained `master`: the first parent of the merge is an
  ancestor of the head, and the head and the pushed commit have the same tree.
  The pull request run tested the merge of that head into an earlier or equal
  `master`, which is then the same tree, with the workflow files of that tree.
- The merge changed nothing that defines the checks: `.github/`, `scripts/`,
  `Makefile`, `.tool-versions`, `rust-toolchain.toml`, and `.config/` are the
  same in the pushed commit and in its first parent. A pull request that changes
  them is tested again after it merges.
- A completed, successful CI run for the `pull_request` event exists at that
  head, linked to that pull request with base `master`. A run without that link,
  as a fork's pull request has, is no proof.
- In that run the jobs `gates`, `rust`, `deny`, and `gitleaks` each succeeded.
- The pull request always targeted `master`: its timeline has no change of base
  branch. A timeline that cannot be read in full is no proof.

Equal trees alone are not proof. When the head does not contain `master`,
`master` can change and change back between the run and the merge, and the run
tested the merge of the head into the `master` of that moment.

Without the proof the run does every job it is eligible for. With it, `gates`,
`deny`, and `gitleaks` still run: `gitleaks` scans the history that the merge
created, `gates` checks action pins against tags that can move, and `deny` reads
advisories published since the pull request run.

Scheduled runs, and manually dispatched runs, never skip. The build cache is
saved only by `rust` jobs on `refs/heads/master`, so a skipped run does not
refresh it; the weekly run does. The CI badge shows the result of the whole
workflow.

### Tag

The Release workflow reuses the test result of the tagged commit. The proof is a
completed, successful CI run for a `push` to `refs/heads/master` at the commit
the Release workflow checked out, which must still be `master` or an ancestor of
it. It waits up to five minutes for a run that is still in progress. A cancelled
run counts only after a re-run succeeds.

With the proof the workflow skips its tests. It always runs the dependency
policy at the tag, because an advisory can appear after the pull request.
Without the proof it runs the tests itself, as before. The builds start only
after the dependency policy passed and either the proof was found or the tests
passed.

### Lookups

The jobs that look for proof read GitHub's API with a read-only token and write
nothing. They pass no title, branch name, or other text from a pull request into
a shell command, and any error or ambiguity means no proof.

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
