@AGENTS.md

## Claude Code specifics

- Discuss with the owner in Vietnamese. Write documents, code, and commits in English.
- Call GPT models directly with the Agent tool (owner, 2026-11-22): `subagent_type` `terra`, `sol`, `astra`, or `luna` from the user-level `~/.claude/agents`, with no `model` override. Those agents have every tool, so a reviewer brief forbids edits and Git. Confirm the family from the model the duolane monitor shows for the child's requests, not from the child's own report; it must match the [roles](AGENTS.md#roles) table.
- Fall back to the [Codex CLI](instructions/workflow.md#codex-cli-fallback) when direct dispatch fails or the monitor shows a model from another family. When GPT is unavailable, start the Claude stand-in from the roles table with the Agent tool model alias `sonnet`, `opus`, or `fable`.
