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
