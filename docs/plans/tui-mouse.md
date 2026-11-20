# Monitor Mouse Controls Implementation Plan

> **For agentic workers:** Follow the repository's [workflow](../../instructions/workflow.md) for the bounded implementation and independent review. The primary owns Git.

**Goal:** Open an existing detail pane with one click on a visible dashboard row.

**Architecture:** Renderers return the visible row bounds and index mapping. The input loop uses those targets and the same snapshot; open details retain the clicked row's identity across refreshes.

**Tech Stack:** Rust, Ratatui `TableState` and test backend, Crossterm mouse capture.

**Spec:** [Monitor controls](../design/monitor.md).

## Global Constraints

- Support left click on Sessions, Recent requests, and Events. Keep existing keyboard navigation and accept no mouse-driven shutdown. No routing, monitor protocol, event classification, or dependency changes.
- Derive row hits from the drawn frame's inner table bounds and actual scroll offset. No clicks through help, setup, or shutdown overlays.
- Restore mouse capture, alternate screen, and raw mode on normal exit and partial startup failure. Use only mock traffic for tests.

## Review Focus

- A scrolled table opens the displayed row, not the unscrolled index. Events resolves through its filtered indices, not its position in Recent.
- Panel borders, table headers, and blank rows must not open detail, especially in short terminals.
- A new request interleaved above the clicked row must not swap the open detail. An evicted row must not open a different one.
- Mock, attached, and integrated monitors share one terminal setup and teardown path.

## Task: Open row details by mouse

**Files:** `src/tui.rs`, add `src/tui/mouse.rs`, `src/tui/sessions.rs`, `src/tui/requests.rs`, `src/tui/overlay.rs`, `docs/design/README.md`.

**Interfaces:** `VisibleRows` stores a body `Rect` and visible index mapping. Its `hit(column: u16, row: u16) -> Option<usize>` maps physical coordinates to the original snapshot index. `MouseTarget` is `Session(usize)` or `Request(usize)`. Sessions and Recent use the post-render `TableState::offset()`; Events retains the source indices from its existing filter. `MonitorApp` resolves a hit using the original snapshot. Mouse-opened detail is keyed by raw `SessionSnapshot.session_id` (`Option<String>`, the existing aggregation key) or raw `CompletedSnapshot.request_id` (`String`), while only sanitized fields reach the terminal. Keyboard navigation retains its existing behavior.

- [ ] Add failing tests using the existing Ratatui test backend and toy snapshots: a clicked visible row after scroll opens the right detail, a filtered Events row resolves its Recent entry, an overlay or blank row ignores a click, and reordering keeps the clicked identity. Run `cargo test --locked --lib tui::` and confirm the new tests fail before implementation.
- [ ] Return row hits from the renderer, using the same row geometry for drawing and hit testing. Wire only left-button `MouseEventKind::Down` through the existing loop. Use the existing empty detail render state when the clicked identity disappears. Add the click hint to footer/help.
- [ ] Pair Crossterm mouse capture with the existing terminal guard; ensure setup failure can reach cleanup and one cleanup error does not skip the other restores. Verify focused Rust tests, then smoke-test `duolane demo` in a disposable pseudo-terminal with mock traffic and exit via keyboard. No live Claude or Codex account calls.
- [ ] Link the design, run `make check`, get an independent review, and land via the existing PR gate. No release or publish in this slice.
