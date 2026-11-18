# duolane: guide for coding agents

Shared rules live here and in `instructions/`. [CLAUDE.md](CLAUDE.md) adds Claude Code details. Read the topic instructions for the work in hand.

## State

`duolane` is Danny's personal local proxy. It lets Claude Code use a Claude subscription and a ChatGPT subscription in one session, chosen per request by model name. It is written in Rust, licensed under MIT, and not affiliated with Anthropic or OpenAI. The repository and CI logs are public.

The code is being migrated to this design. The [foundation plan](docs/plans/foundation.md) owns the order. Until its slices land, parts of the code differ from the design.

## Map

|Question|Entry point|
|-|-|
|What should the product become?|[Architecture](docs/design/architecture.md)|
|What must hold for credentials and inbound requests?|[Security design](docs/design/security.md)|
|Why was a choice made?|[ADRs](docs/adr/README.md)|
|What work remains, in what order?|[Plans](docs/plans/)|
|How should documents and comments be written?|[Engineering standard](docs/standards/engineering.md)|
|Where did inherited code come from?|[ADR 0001](docs/adr/0001-independent-product.md) and [LICENSE](LICENSE)|
|What does the system do now?|Code and tests|

|Doing|Read first|
|-|-|
|Planning, briefing, implementing, reviewing|[Workflow](instructions/workflow.md)|
|Writing Rust|[Rust](instructions/rust.md)|
|Writing or running tests|[Testing](instructions/testing.md)|
|Touching auth, credentials, inbound handling, dependencies, or workflows|[Security](instructions/security.md)|
|Writing documents|[Documentation](instructions/documentation.md)|

Direct owner instructions and platform safety rules come first. An accepted ADR wins over design text, and design wins over a plan. Code and tests describe current behavior; a disagreement with the design is a defect to repair or a gap the plan tracks.

## Roles

|Role|Who|Duty|
|-|-|-|
|Primary|The session the owner talks to|Designs, writes documentation, decides, briefs, verifies, owns Git|
|Implementer|Codex Terra high for routine work, Codex Sol high for hard work|One bounded task in named paths, with tests; never commits|
|Reviewer|Codex read-only: Sol high for Terra's work, Astra high for Sol's work and the primary's work|Independent review of designs, ADRs, and code|

|Codex model|Current id|Claude stand-in when Codex is unavailable|
|-|-|-|
|Terra|`gpt-6.1-terra`|Sonnet|
|Sol|`gpt-6.2-sol`|Opus|
|Astra|`gpt-6.2-astra`|Fable|

Always use the latest model of each family, and update these ids when a newer one ships (owner, 2026-11-18). Astra and Fable design and review only; they never write code. A reviewer always runs on a different model from the author. The owner chooses the primary session's model.

## Rules for every task

1. Start with `git status --short`. Existing changes belong to someone else unless the brief says otherwise.
2. Put security before features and convenience. Never read, print, or commit credential values, including `~/.codex/auth.json`.
3. Keep the Claude route byte-exact and keep reasoning portable across backends, as [architecture](docs/design/architecture.md#invariants) defines.
4. Write no code comments except the exceptions in the [engineering standard](docs/standards/engineering.md#code-comments). Put decisions in the design that owns them.
5. Tests stay hermetic: mock upstreams, toy credentials, and temporary config and state directories.
6. Report exact files and checks, including failures and skipped checks. Never claim verification that did not run.

## Git and GitHub

- Run `gh` as `dannyota`: prefix it with `GH_TOKEN=$(gh auth token -u dannyota)` and pass `--repo dannyota/duolane`. Never run `gh auth switch` or change git config.
- Push with `GH_TOKEN=$(gh auth token -u dannyota) git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push`.
- Use one worktree per task under `.worktrees/`. Every change lands through a pull request.
- The primary commits, pushes, opens pull requests, and merges after green CI and a passing independent review (owner, 2026-11-18). Documentation-only changes need no independent review unless they set a design or ADR.
- Tags, releases, and package publishing need the owner's approval each time.
- Commits are signed. Use Conventional Commits. Messages describe the change only and never mention agents, AI, or automated assistance. Stage named paths; never use `git add -A` or `git add .`.
- The remotes `upstream` (raine/claude-code-proxy) and `fcakyon` are read-only references. Never push to them or open pull requests against them.
