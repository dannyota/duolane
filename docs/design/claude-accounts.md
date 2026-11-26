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
  is selectable only after the login command succeeds. Close any Claude Code
  process using that profile before logging in again. After a successful login
  it prints the account that signed in and warns when another profile holds the
  same account, because the browser signs in whichever claude.ai account is
  already open.
- `duolane accounts use [name]` accepts `default` or a registered profile name,
  or offers them in a menu. On a terminal the menu shows each profile's account
  and moves with the arrow keys; Esc leaves the selection unchanged. Without a
  terminal it reads a number. An unknown or unsafe name is an error. It saves
  only the selected name, atomically, to
  `<duolane config>/claude-accounts/.selected`. Storing it beside the profiles
  scopes it to that config directory, whatever path alias or letter case reaches
  it. `default` restores the original behavior. This command never starts a
  login or changes an open session.
- `duolane accounts list` prints `default` and every registered profile with its
  account, plan, the time left on the stored login, and its
  [plan usage](#plan-usage). It marks the selected profile and notes a profile
  that holds the same account as an earlier row.
- `duolane accounts remove <name>` signs a named profile out with
  `claude auth logout`, then deletes its directory. It asks for confirmation on
  a terminal unless `--yes` is given, and refuses `default`. If logout fails,
  the profile stays. If the profile is still selected after logout, removal
  resets the selection to `default`. Deleting removes the profile's links to
  shared project folders, never the folders they point to. Close any Claude Code
  process using the profile first.
- `duolane claude` still uses the intercept wrapper. If a named account is
  selected, it sets `CLAUDE_CONFIG_DIR` for that child only and considers the
  selected profile's `settings.json` for the model picker. Plain `claude`
  ignores the selection. An explicitly set `CLAUDE_CONFIG_DIR` takes precedence
  and bypasses account selection.
- `duolane accounts switch [name]` saves a new selection and resumes the current
  project's latest session under it in one step.
  [Switch and resume](#switch-and-resume) defines it.

Managed login, selection, launch preparation, and removal take a per-profile
lock outside the profile directory. Login and logout inherit the lock. If
duolane is killed, the lock lasts until the child and any descendants still
holding it exit. On normal completion duolane releases it once the operation is
complete. Removal and `use` serialize changes to the selector, so removing one
profile does not undo a choice of another profile made during logout. Launch
preparation rechecks the selected name after taking its profile lock. A changed
selection stops the launch instead of opening a different login.

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

## Profile identity

A profile's identity is the device, inode, and birth time of its directory, so a
profile removed and created again under the same name gets a new one. Reading it
needs no credential. `accounts login`, `remove`, and `switch` take a hidden
`--expect <identity>`, and `login` takes `--expect-absent`; each checks it under
the profile's lock before changing anything and refuses on a mismatch or when
the file system reports no birth time. `switch` checks the identity again when
the launcher takes the lock before it shares history, so a profile recreated and
selected meanwhile is refused. A refusal says the profile changed and asks to
reload the list and choose again. Without these options the commands behave as
before. The [monitor](monitor-accounts.md#account-actions) passes them.

## Account details

`list`, the `use` menu, and the login report learn which account a profile holds
by running `claude auth status --json` with that profile's `CLAUDE_CONFIG_DIR`.
Claude Code reads its own login and prints no credential. The
[CLI reference](https://code.claude.com/docs/en/cli-reference) documents the
JSON output, the exit code, and the `loginExpiresAt` field. The `email`,
`orgId`, and `subscriptionType` fields were observed on Claude Code 2.1.330 and
are not documented, so every field is optional and a profile without them shows
as signed in or unknown.

- The status and logout children run without the authentication override
  variables, so they describe the stored login, not the shell's override. They
  also run without `DUOLANE_OPENAI_API_KEY`, as the login and launch children
  do.
- Each status run has a 10 second limit and a 64 KiB output limit. The child
  must exit within the deadline with the expected status code for its JSON. A
  failed check shows the profile as unknown. On interruption, duolane stops the
  active status process groups and waits for the children before returning an
  error. The Unix roster uses the locked `libc` version to restore the previous
  signal handlers when collection finishes.
- The monitor reads the same details for its
  [Usage view](monitor-accounts.md#account-details) on a background thread. That
  reader installs no signal handler, because a handler would swallow a signal
  meant to stop the proxy. A cancel flag stops its status process groups
  instead.
- Displayed fields drop control and bidirectional formatting characters and keep
  at most 64 characters, so status text cannot rewrite the terminal.
- Two signed-in profiles are the same account when their full email, compared
  without case, and organization both match. Display truncation does not affect
  that comparison.

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

## Switch and resume

`duolane accounts switch [name] [--next] [-- <claude arguments>]` changes the
selection and starts `duolane claude` under it. It is a manual step for a login
that has run out of usage. It never runs on its own and never changes an open
session. Its menu shows the stored [plan usage](#plan-usage), but neither the
menu nor `--next` uses it to pick a profile. It is available on Unix, like the
other accounts commands.

1. Refuse when `CLAUDE_CONFIG_DIR` is set, because the launch would ignore the
   selection. When `CLAUDECODE` is set, ask for confirmation on a terminal and
   refuse without one. Claude Code sets it to `1` in the processes it spawns and
   in IDE integrated terminals
   ([environment variables](https://code.claude.com/docs/en/env-vars)), so the
   previous session may still be open and writing its transcript.
2. Pick the target. A name accepts `default` or a registered profile, as `use`
   does. `--next` takes the profile after the current selection in `list` order,
   wrapping around and including `default`. It skips a profile that holds the
   same account as the current selection under the `list` comparison, and a
   profile whose status check reports it signed out. A failed status check
   neither matches nor skips. Without a name or `--next`, show the `use` menu
   with the current selection marked. A name and `--next` together are a usage
   error. A target equal to the current selection, or no candidate for `--next`,
   is an error that changes nothing.
3. Refuse `--environment` before saving the target. Save the target through the
   checked selector, locks, and atomic write that `use` uses. Print the profile
   name and its account.
4. Run the `duolane claude` launch path with the arguments after `--`. If they
   hold no session flag before their own `--`, prepend `--continue`. Session
   flags are `-c`, `--continue`, `-r`, `--resume`, `--session-id`, `--from-pr`,
   and `--teleport`, in separate and `=` forms. Launch checks, refusals, and the
   first-link confirmation stay as
   [One transcript per session](#one-transcript-per-session) defines. Refuse
   before sharing if the saved selection no longer matches the target.

The selection stays saved when the launch fails unless another selection
replaced it. If the target is still selected, retry `duolane claude` with the
same Claude arguments, adding `--continue` if `switch` added it. If the
selection changed or cannot be read, run `duolane accounts switch <target>`
again with the same Claude arguments, keeping `-- <claude args>` if present.

For interactive launches, `--continue` loads the most recent conversation in the
current directory and skips `claude -p`, Agent SDK, and `/loop` sessions
([CLI reference](https://code.claude.com/docs/en/cli-reference#cli-flags)).
duolane leaves that choice to Claude Code and never reads a transcript to find a
session. With several sessions in one project, the most recent may not be the
one that ran out; pass `-- --resume <session-id>` instead. duolane cannot see a
previous process in another terminal; close it first.

## Plan usage

`accounts list` and the `use` menu show how much of each account's five hour and
seven day allowance is used. duolane reads this from responses that already pass
through the proxy. It never asks Anthropic for usage and never reads a Claude
credential to do so.

- Anthropic answers a message request with
  `anthropic-ratelimit-unified-5h-utilization`,
  `anthropic-ratelimit-unified-5h-reset`, and the same pair for `7d`. The
  utilization is treated as a fraction of the window and the reset as epoch
  seconds. First-party documentation does not describe these headers. Their
  names were read from the Claude Code 2.1.330 and 2.1.332 programs. The units
  and a live response were not checked.
- The Claude route reads those headers from each upstream response and forwards
  the response unchanged. It ignores a value that is missing, repeated, not a
  finite number, negative, or above 100.
- A named launch tells the proxy its profile in the proxy user name:
  `duolane claude` sets `HTTPS_PROXY` to
  `http://duolane.<name>:<password>@127.0.0.1:<port>`. `Proxy-Authorization`
  ends at the proxy listener, so the name never reaches Anthropic and no request
  changes. Every request on that `CONNECT` belongs to that profile.
- A connection with the plain user `duolane`, and every request on the plain
  HTTP listener, counts as `default`. A session started with an explicit
  `CLAUDE_CONFIG_DIR` therefore also counts as `default`.
- The proxy keeps the newest values for at most 64 profiles and saves them to
  `claude-usage.json` in the state directory, an owner-only file of at most 64
  KiB, written off the request path. A process mutex and a file lock serialize
  each read, merge, and write. When two values carry the same second, the later
  one wins. `accounts list` ignores the file when it is not a regular owner-only
  file, and ignores an entry whose name is not a valid profile name or whose
  value is out of range.
- A value changes only when that account sends a request through the proxy.
  `SEEN` shows its age. Percentages round down, so 100% means full. A window
  shows 0% once its reset has passed, or, when it has no reset, once its full
  observation is one window length old. A response that omits a window drops its
  previous value if that value has no reset.
- `STATUS` summarizes the newest values. `limited` means a window is full and
  has not reset by the rule above; it shows the time until the latest such reset
  when every full window has one. `available` means at least one window was seen
  and none is limited. `unknown` means the proxy has no values for the account.
  On a color terminal, `available` is green, `limited` is bold red, and
  `unknown` is dim. `STATUS` is an observation, not a reason to change accounts
  automatically.
- Profiles that hold the same account share the newest values. The total line is
  the mean over the accounts the proxy has seen, counting each account once, so
  it assumes equal plans.

## Alternatives and evidence

A one-shot `duolane claude --account <name>` would leave the exhausted login
selected for the next launch, so `switch` saves the choice. Switching on a quota
error inside the proxy would need duolane to hold or substitute Claude
credentials, which this design forbids.

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
the same file. A third check on Claude Code 2.1.332 quit a session under the
default login and ran `duolane accounts switch` to a named profile: the launch
linked the folder after consent and `--continue` resumed that session. Neither
check establishes cross-account compatibility for every model or stored tool
state.

`claude auth login` stores the login without completing Claude Code's first-run
setup. Observed on Claude Code 2.1.332, the first interactive launch of such a
profile runs that setup, including the login method screen, even though
`claude auth status` reports the profile as signed in. duolane does not edit
Claude Code's state to skip it.
