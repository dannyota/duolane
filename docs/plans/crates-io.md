# Crates.io Publishing Implementation Plan

> **For agentic workers:** Follow the repository's [workflow](../../instructions/workflow.md). The primary owns Git and the independent reviews. Steps use checkbox syntax for tracking.

**Goal:** Let the owner verify and publish a released `duolane` tag from GitHub Actions without a long-lived registry token.

**Architecture:** A single manual workflow has an unprivileged preflight job and a separately approved publish job. One local metadata validator makes release gates testable without network access.

**Tech Stack:** GitHub Actions, pinned Rust and actions, Cargo, Python `tomllib` and `unittest`, GitHub CLI.

**Spec:** [Release publishing](../design/release.md).

## Global Constraints

- Only a `workflow_dispatch` on `refs/heads/main` in `dannyota/duolane` can run preflight or publish. The `publish` boolean defaults to `false`.
- Require `vMAJOR.MINOR.PATCH`, a tag commit in the dispatch commit's history, a matching standalone crate with local manifest file paths, a complete GitHub Release, and the expected registry owner.
- Only the approved, cache-free `crates-io` job has `id-token: write`. Do not emit tokens or inject dispatch input into shell code. Do not change `release.yml`, the crate version, or external settings in this pull request.

## Task: Gate a single crate release

**Files:** `.github/workflows/publish-crates.yml`, `.github/workflows/ci.yml`, `scripts/check-crate-release.py`, `scripts/test/check-crate-release-test.py`, `docs/design/README.md`.

**Interface:** `validate(tag: str, commit: str, manifest: dict, release: dict, runs: list, owners: dict, artifacts_dir: Path, tree: bytes) -> None` raises `ValueError` on invalid metadata or differing digests. The CLI takes `TAG COMMIT MANIFEST RELEASE_JSON RUNS_JSON OWNERS_JSON ARTIFACTS_DIR TREE`, where `TREE` contains `git ls-tree -r --full-tree -z` for the verified commit. It emits a fixed error for invalid input. `verify` passes only the validated 40-digit commit and numeric toolchain as job outputs.

## Review Focus

- A branch with the same name does not replace the tag; an annotated tag resolves to its commit. A tag moved while approval was pending blocks publishing.
- A published release can have `targetCommitish: main`. Require a successful `release.yml` push run with both the exact tag and commit instead.
- Match release asset digests to files from that exact run. An uploaded asset with a different digest does not count even if the name and checksum file are present.
- Crates.io ownership must list the expected user, not a team. An explicit publish requires an immutable release and confirmed absence of that version before OIDC. Preflight-only can inspect the existing mutable `v0.1.0` release.
- A dispatch from a fork or a non-`main` branch does not enter preflight checks. A publish requires protected exact `main` and a well-formed owner approval. A tagged symlink or Cargo config, a publish rerun, and the default input must never reach token exchange.

- [x] Add hermetic `unittest` cases that carry a valid `v0.1.0` through `validate()` and reject a wrong manifest version, a different release-run commit, a swapped asset digest, or a team owner. Run `python3 -I scripts/test/check-crate-release-test.py` and confirm it fails before the validator exists.
- [x] Implement `validate()` and its CLI. Match `duolane`, `dannyota`, the tag version and exact successful push run. Require `isDraft == false` and all 12 `uploaded` assets from the existing build matrix, each with the SHA-256 of the expected file from its named build artifact. Reject malformed data without dumping API responses. Run the test in the CI `gates` job and locally.
- [x] Add manual `publish-crates.yml` with `tag` and `publish` inputs. In `verify`, check out the dispatch SHA with history, validate the tag before interpolation into refs, resolve `refs/tags/$TAG^{commit}`, and require it to be an ancestor of the dispatch SHA. Obtain release, exact successful run, and bounded owner metadata. Fetch the six platform artifacts from that run using pinned `actions/download-artifact` and `actions: read`. Validate from the dispatch tree. For explicit publish, require `isImmutable == true` and HTTP 404 for the exact registry version. Record verified outputs before the final step, which compiles the dry-run package at the detached tag with the pinned Rust.
- [x] Create `publish` with `needs: verify`, explicit opt-in, first run attempt only, the `crates-io` environment, read-only contents/actions, and OIDC. Before auth, use the environment and run approvals APIs to require the configured `dannyota` reviewer and approval from that owner for `crates-io`. Check out `refs/tags/<tag>` with credentials disabled, compare `HEAD` to verified SHA, reject tagged `.cargo/config` and `.cargo/config.toml` (including symlink indirection), and use the validated numeric toolchain explicitly with a new `CARGO_HOME`. Exchange OIDC using pinned `rust-lang/crates-io-auth-action`; pass its token only to `cargo publish --no-verify --locked --registry crates-io` with the built-in token provider.
- [x] Link the release design, run `actionlint`, `make pins`, `make check`, and the hermetic validator tests. Have independent reviewers check the design and exact diff, including disallowed refs, moved tags, and privilege separation. The primary commits and lands only with green `ci`.
- [x] After merge and owner-approved settings setup, dispatch preflight only on `v0.1.0`. It must pass without entering `publish`. Wait for separate owner approval before tagging or uploading a new version.
