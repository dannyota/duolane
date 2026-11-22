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

After a login, duolane prints the account that signed in. The browser signs in
whichever claude.ai account is already open, so `login` warns when the new
profile holds the same account as another. To add a different account, sign out
of claude.ai or open the sign-in link in a private window, then log in to the
same profile name again.

## See your profiles

`duolane accounts list` shows the account, plan, and remaining login time of
each profile and marks the selected one:

```text
   PROFILE   ACCOUNT              PLAN   LOGIN
 ● default   first@example.com    team   26d left
   backup    second@example.com   max    29d left
```

On a terminal, `duolane accounts use` shows the same details in a menu. Move
with the arrow keys, press Enter to select, or press Esc to keep the current
selection.

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

To delete a profile, run `duolane accounts remove <name>`. It asks for
confirmation, signs the profile out with `claude auth logout`, deletes its
directory, and returns the selection to `default` if that profile was selected.
Shared history folders stay in place. Close any Claude Code session that uses
the profile first. The
[account selection design](https://github.com/dannyota/duolane/blob/HEAD/docs/design/claude-accounts.md)
describes the checks and limits.
