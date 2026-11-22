# Claude account selection

`duolane accounts` is an opt-in launcher feature, not a proxy credential pool.
It selects one Claude Code login for the next `duolane claude` process. It never
switches a running process or falls back on a quota error. Codex is unchanged.

## Commands

- `duolane accounts login [name]` takes a name, or asks for one, and runs
  `claude auth login` with `CLAUDE_CONFIG_DIR` set to
  `<duolane config>/claude-accounts/<name>`. A name is 1–32 lowercase ASCII
  letters, digits, hyphens, or underscores, starts with a letter or digit, and
  is not `default`. Reusing a name signs in to that profile again. A new profile
  is selectable only after the login command succeeds. Concurrent managed logins
  for one profile are refused. The lock is released when the login command
  returns, even if it left a background process; if duolane itself is killed,
  the lock lasts until the login command and its children exit. Close any Claude
  Code process using that profile before logging in again.
- `duolane accounts use [name]` accepts `default` or a registered profile name,
  or offers them in a menu. An unknown or unsafe name is an error. It saves only
  the selected name, atomically, to
  `<duolane config>/claude-accounts/.selected`. Storing it beside the profiles
  scopes it to that config directory, whatever path alias or letter case reaches
  it. `default` restores the original behavior. This command never starts a
  login or changes an open session.
- `duolane claude` still uses the intercept wrapper. If a named account is
  selected, it sets `CLAUDE_CONFIG_DIR` for that child only and considers the
  selected profile's `settings.json` for the model picker. Plain `claude`
  ignores the selection. An explicitly set `CLAUDE_CONFIG_DIR` takes precedence
  and bypasses account selection.

With no selection, or with `default`, startup creates no profile or transcript
links. Removing the duolane config directory removes the profiles and the
selection together, so the next launch uses the default login. An invalid
selection or unsafe profile is an error, not a fallback to a different login.
The accounts commands are available on Unix; the existing launcher remains
unchanged on other platforms. First-time transcript linking requires an atomic
no-replace rename, supported on the macOS and Linux builds.

Claude Code owns login, storage, and refresh. duolane never reads, copies,
rewrites, or substitutes a Claude credential. Named login and launch refuse
explicit authentication overrides inherited from the shell, reporting only
variable names. A launch is a choice of config directory, not a guarantee of
billing: Claude Code still applies its own settings and managed policies. Check
`/status` before sending a prompt if those settings override authentication. The
picker reads only an owned, regular, bounded `settings.json` in the named
profile, without following a symlink. If it cannot safely inspect that file, it
skips adding the Codex lineup. The
[Claude Code authentication docs](https://code.claude.com/docs/en/authentication)
describe separation by configuration directory, including macOS Keychain
entries.

## One transcript per session

For a named launch, duolane creates a link for the current project only:
`<profile>/projects/<project>` points to `~/.claude/projects/<project>`. The
original file stays where it is. `duolane claude --resume <session-id>` can
continue it under the selected login without forking. Before the first link,
confirm in a terminal that the named profile may use the displayed transcript
folder and everything in it. Without confirmation, no link is created. The link
and the canonical project path recorded beside it remain for later launches.
Other profile settings and credentials are not shared.

Use
[Claude Code's documented project directory naming](https://code.claude.com/docs/en/sessions#where-transcripts-are-stored)
for the canonical working directory: replace non-alphanumeric ASCII characters
with hyphens. The first version refuses paths with non-ASCII characters or more
than 200 bytes instead of guessing the upstream encoding or hash. Named launches
refuse `CLAUDE_CODE_PROJECT_DIR_NAME`, `--worktree`, and `-w` before linking
rather than guessing the child's history directory. Open an existing worktree
first, then run `duolane claude` there. Other arguments pass through unchanged.
The launcher never reads the transcript.

A launch reuses a link only when both the target and saved canonical working
path match. Distinct paths can have the same Claude Code folder name; the folder
can therefore already contain other history. Approval is for the entire
displayed folder. The saved path rejects later reuse of that link from a
different working directory with the same folder name. The link and its metadata
never replace existing history, files, or a conflicting link. Check directory
owner and permissions; keep named profiles private. Use bounded reads of
regular, owner-only metadata. Do not follow symlinks for profile roots,
metadata, or the target project directory. Named launch refuses a managed login
that is in progress; it does not lock out native Claude Code commands or
programs that edit the profile afterward.

The link also shares project auto memory and other files Claude Code keeps in
that directory. Resuming can send project history under the selected account.
Only share projects that both accounts may access. Close the previous process
before resuming the same session: Claude Code does not guarantee concurrent
writes to one transcript.

## Alternatives and evidence

PR [#73](https://github.com/dannyota/duolane/pull/73) proposed named profiles
and a persistent selector. Keep those ideas, not the server-side token pool or
quota rotation. Copying the login to the original config would replace the
original login and would not copy a macOS Keychain entry. Forking a transcript
would preserve the conversation but not its file or session ID. Linking the
entire `projects` directory would expose more projects than needed.

A live check on Claude Code 2.1.330 used two distinct subscription logins and
one linked project folder. The second login resumed by ID, recalled the first
turn, and appended to the original file without changing its inode or creating a
second transcript. A second check ran the duolane wrapper with the default login
and a profile created by `duolane accounts login`: a session started under the
default login, `accounts use` switched profiles, and `duolane claude --resume`
asked for consent, linked the folder, recalled the first turn, and appended to
the same file. Neither check establishes cross-account compatibility for every
model or stored tool state.
