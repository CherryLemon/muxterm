# ADR 0005: resynchronisation, incremental pane capture and screen refresh

* Status: accepted
* Provenance: our own design, resting on the single-ordered-stream property and the
  watermark rule from [`0003`](0003-snapshot-watermark-and-barrier.md).

## Context

ADR 0003 made the first snapshot trustworthy, but the first implementation treated
it as the end of the story:

* a pane created after the snapshot never got a `PaneState` or a capture, and its
  output was dropped forever;
* `%sessions-changed` and a parser desync only set `NeedsResync`, with no code path
  that could ever return to `Synced`;
* `capture-pane -S -` could exceed the response-line limit; the truncated response
  (the **oldest** history, not the visible screen) was still applied, and a later
  `Done` overwrote the desync flag;
* `TerminalCore::restore` assumed a virgin core, so a second restore painted at the
  old cursor and inherited modes;
* the client never sent `refresh-client -C`, so tmux sized the window from the
  control client's default and the GUI could not drive pane geometry;
* `%pause`, `%continue`, `%raw` and `%config-error` were parsed and then dropped.

## Decision

1. **Snapshot generations.** `RemoteModel::begin_snapshot` starts a generation,
   marks every pane "not ready" (its output is already in the upcoming capture)
   and clears session→window links. `mark_synced` refuses to transition when a
   desync happened in the same generation, and `prune_unseen` removes objects the
   completed snapshot did not report. A desync can therefore never be papered over
   by a later `Done`.
2. **Bounded, fatal captures.** A capture asks for at most
   `max_response_lines - pane_height` history lines (`-S -<n>`, or `-S 0` for the
   visible screen). A response that is nevertheless truncated is
   `SyncError::Truncated`; it is never applied as a snapshot.
3. **Resynchronisation exists.** `ControlSession::resync()` re-runs the sequence.
   `%sessions-changed` and a protocol desync request it automatically when the
   queue is idle, capped by `MAX_AUTO_RESYNCS` so a permanently broken stream
   cannot spin. A sync failure clears the barrier and leaves the model untrusted.
4. **Incremental pane capture.** A pane discovered by `%layout-change` gets its own
   capture + metadata exchange (`RequestTag::PaneRefreshCapture/Meta`) before its
   output is delivered. This is what makes `split-window` work after attach.
5. **Idempotent restore + damage.** `restore` starts with RIS (`\x1bc`), which
   resets grid, history, modes and scroll region, then paints. `TerminalCore`
   exposes `take_damage()` (full or per-line spans) so a renderer can repaint only
   what changed.
6. **Client size and pane geometry.** `resize_client` / `resize_window` send
   `refresh-client -C`; layout changes update `PaneState.width/height` and emit
   `PaneResized`.
7. **Input and flow control.** Mouse/paste/focus encoders live in
   `term_core::input` and every byte reaches the pane through `send-keys -H`.
   `%pause`/`%continue` become events; `set_pane_output` answers them with
   `refresh-client -A`.

## Consequences

* A resync costs one extra round trip per snapshot command and re-captures every
  pane; it is rare (attach, sessions-changed, desync) and correctness wins.
* The GUI must apply `PaneSnapshot` before the next `PaneOutput` for that pane;
  the event order already guarantees this.
* A truncated capture now fails loudly instead of showing stale history. Users with
  very large `history-limit` values see the configured bound, not a wrong screen.
* `PaneState` is only constructible inside `tmux-client` (it carries a private
  `seen` generation); consumers read it. That is deliberate: the model owns the
  snapshot-generation bookkeeping.