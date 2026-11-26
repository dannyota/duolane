# Workflow

The primary designs and decides. Implementers change code. Reviewers judge work they did not write.

## Flow

1. Read the relevant design and accepted ADRs. Create one worktree under `.worktrees/` for the task.
2. A change to architecture, a contract, or a security boundary starts with a design or ADR update that an independent reviewer accepts. Small changes skip this step.
3. Brief one implementer per bounded task: objective, authorities to read, owned paths, forbidden actions, numbered decisions, test requirements, gate commands, and the report contract. Include "Do NOT commit". Ask for minimal tests (owner): one test per rule, with a hostile and a legitimate case, and no exhaustive variants. Keep each brief to one to three decisions; a larger task becomes several briefs in sequence, because a direct implementer agent tends to stop early on a long brief.
4. Failing test first, then the smallest correct change. Hostile input, authorization, races, and bounds belong to the author.
5. The primary runs the gates, reads the risky parts of the diff, and commits.
6. Send the commit SHA to a reviewer the routing selects, with the design, decisions, known bugs, and a regression checklist. Ask for severity, `file:line`, a failure scenario, a fix sentence, and a verdict of MERGE, MERGE-WITH-FIXES, or BLOCK.
7. Paste findings verbatim into a new implementer brief. The same reviewer confirms the fixes.
8. Push, open a pull request, wait for the `ci` check on the exact head, and merge with a merge commit. Delete the branch and the worktree.

Two red CI runs from the same cause mean stop and repair the process. Run one `make check` at a time on this laptop; agents that build in parallel use `CARGO_BUILD_JOBS=4`.

## Codex CLI fallback

Use the Codex CLI only when direct subagent dispatch fails or the duolane monitor shows its requests on a model from another family.


`C=$(ls -d ~/.claude/plugins/cache/openai-codex/codex/*/ | sort -V | tail -1)scripts/codex-companion.mjs`

- Start from the worktree: `node "$C" task --background [--write] --fresh --model <model> --effort high "<prompt>"`. Point the prompt at a brief file in the scratchpad.
- Run `node "$C" status <id>` and `node "$C" result <id>` from the same directory. Poll no more often than every 90 seconds.
- Use `--write` only for implementers. Reviewers run read-only.
- The sandbox may have no network and cannot write outside the worktree. The primary reruns anything that needs either.

## Reviews

Credential handling, inbound request handling, release workflows, and dependency trust always get an adversarial pass. Model these attackers: a web page in the owner's browser, another local process, a host on the network when bound beyond loopback, a hostile or compromised upstream, and hostile text in model output or tool results.

## Reports

A report lists new, changed, and deleted files; checks run with results; checks not run with the reason; deviations from the brief; and open items.
