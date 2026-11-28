# Monitor accounts

The Claude profiles in the [Usage view](monitor.md#usage) of the in-process
monitor: their table, state, account details, and actions.

## Profile table

The in-process monitor lists the Claude profiles in a table with a header, so
every value has a labeled column and nothing needs a key to appear.

- The columns are the saved and suggested marks, `PROFILE`, `ACCOUNT`, `PLAN`,
  `5H`, `WEEK`, `LOGIN`, `STATUS`, `SEEN`, and a note for a shared account. A
  window is a bar of 6 cells, the percent used, and the time until its reset.
- `●` marks the profile saved for the next launch and `★` the suggested one. The
  top border names both marks and the saved profile.
- When the pane is too narrow, columns give way in this order: the note, the
  account beyond 16 cells, `SEEN`, `LOGIN`, the bars, `PLAN`, `ACCOUNT`, and the
  profile name beyond 8 cells. The name, both windows, and the status always
  show.
- Below the rows, a button line shows every action for the focused profile:
  `[Enter Use]`, `[L Sign in]`, `[D Remove]`, `[S Switch]`, `[A Add]`, and
  `[R Refresh]`. A button that does not apply to the focused profile is dim. A
  left click on a button acts as its key, and the keys work in either case.
- The last line is the message line. It shows, first of those that apply: the
  running action, the result of the last action in green or red, the result of a
  selection, the `CLAUDE_CONFIG_DIR` override, and otherwise the focused profile
  in full with its account, login time, and the age of the details.
- When the pane is too short, the header goes first, then the message line. The
  rows scroll to keep the focused profile in view above the buttons.

## Quick switch

`n` picks another Claude profile for the next launch from either view. It is a
shortcut for focusing a row and pressing `Enter`, so it saves a selection and
never starts a launch or changes a running session.

- `n` opens Usage and the profile confirmation for a target. The first press
  takes the suggested profile, or without one the profile after the saved one in
  table order. Each further press moves to the next target and wraps around.
- A target is never the saved profile, a profile that holds the same account as
  the saved one, or a profile whose login is known to be signed out or expired.
  With no target the message line says so and nothing opens.
- The confirmation names the profile, its account, and the percent used of both
  windows when they are known. `Enter` and `Esc` keep the rules of the profile
  confirmation.
- The key does nothing in the attached and demo monitors, when
  `CLAUDE_CONFIG_DIR` overrides the selection, while a name is typed, and while
  an account action waits for confirmation.

## Profile state

The table shows the state of each Claude profile from the plan usage the monitor
already loads.

- The status follows the `STATUS` rules of
  [plan usage](claude-accounts.md#plan-usage): `available` in green, `limited`
  in red with the time until the reset, and `unknown` in dim text.
- `SEEN` gives the age of the values. After 15 minutes it reads `stale` with the
  age, in yellow, as the ChatGPT report is marked. A stale value is still shown,
  because an idle account sends nothing new.
- The bottom border of the pane gives the total over all accounts, with the text
  and the rules of the `accounts list` total.
- One profile is marked `★` as suggested: the available profile whose fuller
  window has the lowest percent used. An earlier row wins a tie. A profile is
  skipped when its status is not `available`, when it holds the same account as
  an earlier row, or when its login is known to be signed out or expired. A
  roster of one profile has no mark. The mark is advice only. It never changes
  the selection and never starts a launch.

## Account details

The in-process monitor on Unix shows what `accounts list` shows for each
profile: the account email, the plan, the time left on the stored login, and a
note when a profile holds the same account as an earlier row. Profiles that hold
the same account share the newest plan usage, as in `accounts list`.

- The details come from the
  [account details](claude-accounts.md#account-details) reader, with its limits
  on time, output, and displayed text. Each check runs `claude auth status` once
  per profile, so it runs on a background thread and never blocks drawing.
- A check starts when Usage opens and the last result is older than 5 minutes,
  and on `r`. Only one check runs at a time. An `r` or a finished action during
  a check starts one more check when it ends. The last result stays on screen
  while a check runs.
- The message line states the age of the details, such as
  `Accounts checked 2m05s ago`, or `Accounts checking`, apart from the plan
  usage, which reloads every 30 seconds. A profile with no details shows `-`.
- The login time is yellow under 3 days and red when the login expired. A
  signed-out profile shows `not signed in` as its account.
- The check installs no signal handler. Leaving the monitor cancels it, stops
  its status process groups, and waits for the thread.
- The details stay in the memory of the monitor that read them. They are never
  written to a file, the log, or the monitor snapshot. The attached and demo
  monitors never run the check and never show an account email.

## Account actions

The in-process monitor on Unix can run the account commands for the focused
profile. Each action runs the `duolane` program itself as a child process with
fixed arguments, so it keeps the checks, locks, and prompts of the
[command](claude-accounts.md#commands). The monitor builds no shell command. The
only text taken from the keyboard is a new profile name, accepted only when it
passes the profile name rule.

| Key | Action            | Command                        | Mode     |
| --- | ----------------- | ------------------------------ | -------- |
| `a` | Add a profile     | `accounts login <name>`        | handoff  |
| `l` | Sign in again     | `accounts login <name>`        | handoff  |
| `d` | Remove a profile  | `accounts remove <name> --yes` | captured |
| `s` | Switch and resume | `accounts switch <name>`       | handoff  |

- Every action asks for confirmation in an overlay that names the action, the
  profile, and the account last seen for it. `Enter` confirms under the size
  rule of the profile confirmation and `Esc` cancels. `a` first asks for the new
  name and refuses a name that is already registered.
- On confirmation the monitor reads the roster again. It runs the action only
  when the profile is still registered, or still absent for `a`. The overlay
  records the profile's [identity](claude-accounts.md#profile-identity) when it
  opens and passes it to the command, which refuses under its profile lock if a
  profile of that name was removed and created again meanwhile. Account details
  are cached by name and identity, so a recreated profile never shows the old
  account.
- `l` and `d` are refused for `default`, which Claude Code manages. `s` is
  refused for the saved profile and when `CLAUDE_CONFIG_DIR` overrides the
  selection. `s` resumes in the directory where `duolane serve` started, and the
  confirmation names that directory.
- Only one action runs at a time. Another action key during a run reports that
  the monitor is busy.
- A captured action runs on a background thread with standard input closed, in
  its own process group. Its output is kept up to 64 KiB with control characters
  and escape sequences removed. One loop watches the deadline, the exit, and the
  output, so a process that keeps the pipe open cannot delay the result. After
  60 seconds or 64 KiB the monitor kills the process group and reports a
  failure; success needs exit status 0 within both limits. The message line
  shows the action while it runs, then the result with the last line of output.
- When the monitor stops, by a key, a shutdown, or `SIGTERM`, `SIGINT`, or
  `SIGHUP`, it kills a running captured action's process group and waits for it
  before the proxy exits. A signal that arrives during a handoff is handled when
  the handoff returns, because the child owns the terminal until then.
- A handoff action needs the terminal for a browser sign-in or for Claude Code.
  It uses the controlling terminal, `/dev/tty`, and is refused with a message,
  leaving the dashboard as it is, when that terminal cannot be opened. The
  child's input is the monitor's own readable standard input, output, or error
  when that descriptor is the same controlling terminal, and `/dev/tty` only
  when none is. macOS kqueue rejects a descriptor opened through `/dev/tty` with
  `EINVAL`, and Claude Code, which runs on Bun, watches its input with kqueue,
  so a sign-in given `/dev/tty` crashes before it starts. The monitor saves the
  terminal attributes, leaves the alternate screen, turns off raw mode and mouse
  capture, and runs the child in its own process group as the foreground group
  of the terminal. `Ctrl-C` therefore reaches only the child, never the proxy. A
  child stopped with `Ctrl-Z` is continued at once. When the child exits, the
  monitor takes the terminal back, restores the saved attributes, waits for
  `Enter`, and redraws. If it cannot take the terminal back, it restores the
  attributes and stops without drawing. The proxy serves throughout.
- After an action ends, however it ends, the monitor reloads the roster and the
  plan usage and starts an account details check. A failed action changes no
  monitor state besides the message line.
- A click on a button only opens the confirmation or the name entry, as its key
  does. While the new name is typed, every key goes to the name, so `q`, `1`,
  and `r` do not act on the monitor, and clicks are ignored.
- The attached and demo monitors show no table, no buttons, and run nothing.
