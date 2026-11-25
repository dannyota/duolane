# Monitor controls

A left click on a visible row in Sessions, Recent requests, or Events opens its
detail. Events opens the same request detail as Recent requests. The selected
detail follows the clicked identity if new activity reorders rows; if it leaves
the snapshot, show that it is no longer available.

A session's provider, model, and effort come from its request with the largest
known input token count, including active requests, with ties resolved by latest
start time; if all counts are unknown, use the latest request with a model, or
the latest request if none has a model.

## Approach

Use the existing detail panes and add mouse input to the dashboard. Keeping
clicks separate from keyboard navigation would duplicate selection logic. Single
click opens detail; `Esc` closes it. Existing keyboard shortcuts keep working.
Ignore clicks on table headers, empty space, and while help, setup, or shutdown
overlays are visible. No pointer support for Active requests.

Read hit targets from the visible table rows of the last rendered frame,
including the scroll offset and the Events filter. Enable mouse capture only
while the monitor owns the terminal, and restore the terminal on setup failure
and exit. Mouse input does not change the snapshot protocol.

## Events

The Events table shows the latest 12 abnormal completions, excluding expected
thread retries. Its title shows `Retry requested: N` when the current recent
request snapshot contains such retries. They remain clickable in Recent requests
with their original HTTP 400 and detail message. The count reports requests to
retry, not completed retries. Failure totals do not change.

Mark a retry only where the proxy generates the `message-threads` capability
rejection on the Codex route. Do not classify retries by matching upstream error
text. Carry the marker as a default-false field on completed monitor snapshots
so older snapshots still render, and keep the monitor protocol version, HTTP
response, and request routing unchanged.

## Column sizing

In expanded and wide request tables, Model and Endpoint fit the table values,
with caps of 36 and 12 cells. Expanded and wide Sessions use the same Model cap.
Project can grow to 40 cells while preserving the flexible column's minimum
space. Active requests leave spare space after the metrics; Recent requests give
it to Details when present. Widths stay stable while scrolling. Long model and
project values are ellipsized.

## Views

The header shows two tabs, `1 Overview` and `2 Usage`, with the open one
highlighted. The key or a left click on a tab opens that view. The footer hints
for `q`, `?`, `b`, `Tab`, `1`, `2`, and `r` are click targets that act as their
keys. The `Enter` and navigation hints are not targets, because they act on a
selection. A click on a tab or a hint is ignored while the help, setup,
shutdown, or profile confirmation overlay is visible, while a banner covers the
footer, and when the terminal size differs from the frame the targets came from.
Only a table row opens a detail.

Dim text uses a color with a contrast of at least 4.5 to 1 on the background.

## Overview

From the top: the lane summary, Sessions, Active requests, Recent requests, and
Events.

- The lane summary has one cell for Claude, one for Codex, and one for
  [connections](#connections). A lane cell counts requests by provider, so the
  Codex cell includes requests sent with an OpenAI API key. It shows the active
  and [queued](#request-phases) request counts, and the median Wait and the
  pooled rate of the lane's recent requests.
- The Codex cell shows the bars of the [reported limits](#reported-limits),
  which belong to the ChatGPT subscription alone. A bar is labeled `5h` or
  `week` only when its window length is 300 or 10,080 minutes, and `Primary` or
  `Secondary` otherwise. The Claude cell of the in-process monitor shows the
  bars of the profile with the newest [plan usage](#usage) observation and names
  it. The attached and demo monitors show no Claude bars.
- Sessions is as tall as its rows, up to 30% of the height. Events is one line
  while it has no rows, and up to 25% of the height otherwise. Active requests
  takes the rows it needs, up to half of what is left, and Recent requests takes
  the rest.
- Sessions shows a Queue count after the active count. The row for requests
  without a session counts them too.
- Below the expanded width each summary cell is one line without bars, and the
  tables omit Queue, the timeline, Wait, First, and the bars.

A pooled rate is the output tokens counted during generation, summed over the
requests that report both tokens and a generation time, divided by the sum of
those times.

### Request phases

The request status in the snapshot keeps its values. The dashboard derives a
phase for an active request from the status and the times below:

| Phase        | From                                     | Until                    |
| ------------ | ---------------------------------------- | ------------------------ |
| `queued #n`  | It needs a new WebSocket handshake       | Its handshake starts     |
| `connecting` | It is sent upstream, or leaves the queue | The first upstream event |
| `thinking`   | The first upstream event                 | The first output         |
| `streaming`  | The first output                         | The request ends         |

- The queue rule applies to each attempt. An attempt is queued from the moment
  it needs a new handshake, before it waits for one of the handshake slots,
  until its handshake starts or the request is cancelled. `n` is its place among
  the queued attempts in arrival order. An attempt over HTTP or on a reused
  socket is never queued.
- A request shows `queued #n` whenever its current attempt is queued, including
  a retry after the first upstream event.
- The first upstream event is recorded once for the request. A retry does not
  move it, so the timeline counts the time a retry takes, with any second wait
  in the queue, as `thinking`.
- The first output is the first content delivered to the client that has
  meaning: a text or reasoning delta, or the start of a tool block. Framing
  alone is not output, so an empty completion and a buffered response have no
  first output.
- A status with no phase, such as compaction, shows as it does today.

Active requests shows the phase and a timeline of one cell per second: wait
cells for `queued` and `connecting`, then `thinking` cells, then `streaming`
cells, each in its own color. A timeline longer than its column is scaled to
fit. The table title carries the legend.

Recent requests shows three times from the arrival of the request: Wait, to the
first upstream event; First, to the first output; and Total. A time that was not
measured shows `-`. A bar repeats Wait and the remaining time at one cell per
second. Rate is unchanged: each request keeps its token rate over its generation
time, or its byte or event rate when it reports no tokens, so Rate excludes
Wait.

The queue place, the first upstream event, and the first output are optional
fields on active and completed requests in the snapshot. A snapshot without them
shows `-`, and the monitor protocol version does not change.

### Connections

The Codex transport reports counters for its
[WebSocket handshakes and pool](codex.md#transport-and-retries): queued
requests, handshakes in flight, the current start spacing, and, since the proxy
started, WebSocket requests sent on a reused socket and on a new one, HTTP
fallbacks, and origin 403 responses. The cell shows the queue length, handshakes
in flight, the most starts per second the spacing allows, the share of WebSocket
requests that reused a socket, and the last two counts under the labels
`HTTP fallbacks` and `origin 403`. Its border is yellow while the queue is not
empty.

The counters are an optional snapshot field. They hold numbers only: no URL,
header, account, or conversation value.

## Usage

`1` opens Overview and `2` opens Usage. The view shows the Claude profiles and
the ChatGPT subscription with their limits, then Speed by model and Tokens by
session. The in-process monitor adds the [profile state](#profile-state), the
[account details](#account-details), and the
[account actions](#account-actions).

- A limit window is a bar with its percent used and reset time. The bar is
  yellow from 50% and red from 80%.
- The in-process monitor loads [plan usage](claude-accounts.md#plan-usage) by
  profile name with the reader and rules of `accounts list`, so a Claude window
  shows 0% once its reset has passed. It loads when the monitor starts, on `r`,
  and every 30 seconds. The attached and demo monitors show `Unknown` for Claude
  profiles, and no profile name or plan usage crosses the snapshot.
- The stale rule of the [reported limits](#reported-limits) applies to the
  ChatGPT subscription only.
- Speed by model groups the recent requests in the snapshot by provider, model,
  and effort. Each row shows the request count, the median Wait, the median
  generation time, the median output tokens, the pooled rate, and a bar of the
  two medians at one cell per half second.
- Tokens by session lists each session's input and output tokens and its share
  of all output tokens, as a number and a bar.
- The bars take the width the other columns leave. The two tables share the
  height below the limits by their row counts, and a table that does not fit
  shows the rows that fit and the number left out.
- The in-process monitor shows the Claude profiles as the
  [profile table](#profile-table), one line per profile.
- The ChatGPT pane shows one line per window when it has room. Otherwise it uses
  compact lines. A compact line gives for each window a short label, the
  percent, and the time left until the reset, such as `5h 42% 1h54m`; then a
  bar, only when cells remain. A window with no reset time shows `-` there, and
  one whose reset has passed shows `passed`. The pane has its name in its title,
  so its compact lines start at the windows, on one line or one line per window.
  A non-standard ChatGPT window uses its length as the label, such as `120m`.
  The stale marker is never dropped while a value shows.
- The confirmation for a profile change is an overlay, so it fits a terminal of
  80 by 24. `Enter` saves only when the terminal still has the size of the frame
  that showed the confirmation. The Claude pane keeps showing the saved profile
  and the `CLAUDE_CONFIG_DIR` message.

Arrow keys or a row click move the focus without changing the selected profile.
`Enter` opens a confirmation for a different Claude profile; `Enter` confirms it
and `Esc` cancels. Display both the focused row and the profile selected for the
next launch. Keep the existing shutdown and help controls in both views. At
narrow widths the Claude profiles and the ChatGPT subscription stack; at wide
widths they sit side by side. The read-only monitors sit side by side from 90
cells. The in-process monitor does so from 180 cells, because its table needs
the width, and gives the ChatGPT pane 52 cells.

Only the in-process monitor on Unix can change the profile. It reuses the
checked selector and locks from [`accounts use`](claude-accounts.md#commands)
and rejects a confirmation if the selection changed since it was displayed. It
never changes a running session, bypasses `CLAUDE_CONFIG_DIR`, rotates accounts,
or changes Codex source order. Disable the choice if that variable is set in the
monitor's environment and explain the override. The attached and demo monitors
do not read or modify the local account store. They do not receive profile names
or selection state over HTTP.

`r` reloads the local names and saved choice. Entering Usage also reads the
local roster. These reads make no provider request. A registered profile is not
evidence that its login is still valid; [account details](#account-details)
report the login separately.

### Profile table

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

### Profile state

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

### Account details

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

### Account actions

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
  when the profile is still registered, or still absent for `a`. The command
  itself checks the name again under its locks.
- `l` and `d` are refused for `default`, which Claude Code manages. `s` is
  refused for the saved profile and when `CLAUDE_CONFIG_DIR` overrides the
  selection. `s` resumes in the directory where `duolane serve` started, and the
  confirmation names that directory.
- Only one action runs at a time. Another action key during a run reports that
  the monitor is busy.
- A captured action runs on a background thread with standard input closed, in
  its own process group. Its output is kept up to 64 KiB with control characters
  and escape sequences removed. After 60 seconds the monitor stops the process
  group and reports a timeout. The message line shows the action while it runs,
  then the result with the last line of output.
- A handoff action needs the terminal for a browser sign-in or for Claude Code.
  The monitor leaves the alternate screen, turns off raw mode and mouse capture,
  and runs the child in its own process group as the foreground group of the
  terminal. `Ctrl-C` therefore reaches only the child, never the proxy. A child
  stopped with `Ctrl-Z` is continued at once. When the child exits, the monitor
  takes the terminal back, waits for `Enter`, and redraws. The proxy serves
  throughout.
- After an action ends, however it ends, the monitor reloads the roster and the
  plan usage and starts an account details check. A failed action changes no
  monitor state besides the message line.
- A click on a button only opens the confirmation or the name entry, as its key
  does. While the new name is typed, every key goes to the name, so `q`, `1`,
  and `r` do not act on the monitor, and clicks are ignored.
- The attached and demo monitors show no table, no buttons, and run nothing.

### Reported limits

Show the last usable ChatGPT subscription report that an authenticated streaming
Codex Messages attempt received from `codex.rate_limits` over HTTP or WebSocket.
Buffered requests do not update the view. Its windows report percent used,
window length, and reset time when present. Keep one report in memory and record
when it was observed. Bind it to the authenticated account using the existing
private account binding. Capture the generation when each attempt binds its
credentials, before reading events. Clear the report when a later bound attempt
changes the account; publish only if the captured generation is still current.
Do not show a report for an unbound attempt. An account change outside the proxy
is not known until the proxy observes it, so the view says last reported, not
live usage.

Expose only those bounded window values and the observation time as an optional
monitor snapshot field. Do not include account identifiers, account bindings,
raw headers/events, login state, or profile metadata in that field. Keep the
monitor protocol version and request responses unchanged. Older snapshots
without the field show `Unknown` for the ChatGPT subscription.

Show `Unknown` for missing or invalid percent/reset fields, including Claude
profiles with no verified attribution. A report with no usable window leaves the
previous usable report and observation time unchanged. Never copy one account's
numbers to another or infer a limit from tokens. Show a report as stale after 15
minutes or when a reported reset passes; a reset does not imply zero usage.
Quotas are observations, not reasons to automatically change accounts. OpenAI
API keys are not ChatGPT subscription windows and never contribute to this
report.
