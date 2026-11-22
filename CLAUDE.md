@AGENTS.md

## Claude Code specifics

- Discuss with the owner in Vietnamese. Write documents, code, and commits in English.
- Call GPT models directly with the Agent tool (owner, 2026-11-22): `subagent_type` `terra`, `sol`, `astra`, or `luna` from the user-level `~/.claude/agents`, with no `model` override. Those agents have every tool, so a reviewer brief forbids edits and Git. Confirm the child's runtime model id belongs to the family the [roles](AGENTS.md#roles) table names.
- Fall back to the [Codex CLI](instructions/workflow.md#codex-cli-fallback) when direct dispatch fails or the runtime model cannot be confirmed. When GPT is unavailable, start the Claude stand-in from the roles table with the Agent tool model alias `sonnet`, `opus`, or `fable`.
