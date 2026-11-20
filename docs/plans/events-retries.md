# Expected Thread Retries Implementation Plan

> **For agentic workers:** Follow the repository's [workflow](../../instructions/workflow.md). The primary owns Git and verification.

**Goal:** Expected thread retries no longer displace genuine errors from Events.

**Architecture:** An explicit monitor completion marker originates at the synthetic Codex rejection. Snapshot consumers default it to false. The Events renderer counts marked retries and filters them before limiting visible rows.

**Tech Stack:** Rust, Serde, Ratatui test backend, existing mock providers.

**Spec:** [Monitor Events](../design/monitor.md#events).

## Global Constraints

- Only the existing internal `message-threads` rejection marks a retry. Keep the original HTTP 400, body, logs, request dispatch, request status, and session totals.
- `CompletedSnapshot.retry_requested: bool` defaults to false when absent. Keep `PROTOCOL_VERSION` and the public Rust `MonitorEvent` variants unchanged. No upstream/account interaction.
- `Events` shows `Retry requested: N` only for a nonzero count in the current recent snapshot. Filter marked rows before the 12-row cap. Do not change Recent requests or selected detail behavior.

## Review Focus

- An ordinary failed request with the same error text remains in Events.
- A true failure behind 12 generated retries stays visible and maps to the correct Recent entry when clicked.
- A legacy remote snapshot without the marker still deserializes and is not reclassified.

## Task: Separate expected retries in Events

**Files:** `src/server/messages.rs`, `src/monitor.rs`, `src/monitor/store.rs`, `src/monitor/snapshot.rs`, bounded monitor fixtures in `src/monitor/mock/requests.rs`, `src/tui/cells.rs`, `src/tui/requests.rs`, `src/tui/mouse_tests.rs`, `tests/server.rs`.

**Interfaces:** `MonitorHandle::request_retry_requested(request_id: impl Into<String>, error: impl Into<String>)` completes with HTTP 400 and existing failed status. A typed internal completion marker propagates into `CompletedRequest.retry_requested` and `CompletedSnapshot.retry_requested`.

- [ ] Add red tests: extend the existing synthetic thread rejection assertion to assert the marker and preserve the HTTP/call checks; verify a lookalike text failure still appears after 12 marked retries and its click opens the correct Recent detail; verify missing-marker snapshots default false.
- [ ] Wire the typed marker only on the existing rejection path. Extend the existing store completion paths without changing accounting. Render the count in the Events title and leave marked requests in Recent.
- [ ] Run focused server and TUI/monitor tests, `make check`, and a mock terminal smoke without live accounts. Independent design and code review before merge.
- [ ] Land the Events PR via green `ci`. Prepare the next patch release separately, with approval before any tag, release, or crates.io upload.
