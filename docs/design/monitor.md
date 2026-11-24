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

## Accounts

`1` opens Overview and `2` opens Accounts. Arrow keys or a row click move the
focus without changing the selected profile. `Enter` opens a confirmation for a
different Claude profile; `Enter` confirms it and `Esc` cancels. Display both
the focused row and the profile selected for the next launch. Keep the existing
shutdown and help controls in both views. At narrow widths the list and limits
stack; at wide widths they sit side by side.

Only the in-process monitor on Unix can change the profile. It reuses the
checked selector and locks from [`accounts use`](claude-accounts.md#commands)
and rejects a confirmation if the selection changed since it was displayed. It
never changes a running session, bypasses `CLAUDE_CONFIG_DIR`, rotates accounts,
or changes Codex source order. Disable the choice if that variable is set in the
monitor's environment and explain the override. The attached and demo monitors
do not read or modify the local account store. They do not receive profile names
or selection state over HTTP.

`r` reloads the local names and saved choice. Entering Accounts also reads the
local roster. These reads do not run a login/status command or make a provider
request. A registered profile is not evidence that its login is still valid.

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
