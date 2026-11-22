# Claude Accounts

On macOS and Linux, duolane can keep more than one Claude Code login and choose
which one the next `duolane claude` launch uses. Each named profile is a
separate Claude Code configuration directory, signed in with Claude Code's own
login. duolane never reads, copies, or rotates a Claude credential.

```bash
duolane accounts login backup
duolane accounts use backup
duolane claude
```

`login` runs `claude auth login` for the profile and registers it only after the
login succeeds. Sign in again with the same name to replace that profile's
login. `use` saves the choice for the next launch; it does not change an open
session. `duolane accounts use default` returns to your original login. Without
a name, `login` asks for one and `use` shows a menu.

Plain `claude`, and any launch with `CLAUDE_CONFIG_DIR` set, ignores the
selection. A named launch refuses shell variables that would override the login,
such as `ANTHROPIC_API_KEY` or `CLAUDE_CODE_OAUTH_TOKEN`. Claude Code settings
and managed policies can still change authentication; check `/status` if you use
them.

## Resume a session under another account

Start a session under one login, quit it, then switch and resume it by ID:

```bash
duolane accounts use backup
duolane claude --resume <session-id>
```

The first named launch in a project asks you to type `yes` before it shares that
project's history folder in `~/.claude/projects/` with the profile. The resumed
session continues in the same transcript file, with the same session ID.

Sharing covers the whole displayed folder, including project auto memory, and
sends that history under the selected account. Share only projects both accounts
may use. Close the previous Claude Code process before you resume the same
session. Named launches do not support `--worktree` or
`CLAUDE_CODE_PROJECT_DIR_NAME`; open the worktree first and run `duolane claude`
there.

## Files

| Path                                          | Holds                             |
| --------------------------------------------- | --------------------------------- |
| `~/.config/duolane/claude-accounts/<name>/`   | The profile's Claude Code config  |
| `~/.config/duolane/claude-accounts/.selected` | The selected name, or `default`   |
| `<profile>/projects/<project>`                | Link to the shared history folder |

To delete a profile, select `default`, then remove its directory. The
[account selection design](https://github.com/dannyota/duolane/blob/HEAD/docs/design/claude-accounts.md)
describes the checks and limits.
