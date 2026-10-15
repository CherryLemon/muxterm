# Snapshot and resynchronisation

**Spec version:** `state-sync/0.1`.

The hard part of a tmux GUI client is not rendering: it is restoring *state* after
a reattach, and knowing when the restored state stops being true.

## 1. What must be restored

| Item | Source | Notes |
|---|---|---|
| sessions, windows, panes, indices, ids | `list-sessions`, `list-windows -a`, `list-panes -a` | formats are fixed in `tmux_client::sync` |
| layout tree (including multi-child splits) | `#{window_layout}` | parsed, never guessed from pixels |
| screen contents | `capture-pane -p -e -q -N -S - -E -` | `-e` keeps escape sequences, `-N` keeps row geometry, `-J` is deliberately absent (see §5) |
| cursor position, alternate screen, history size/limit | `display-message -p -t <pane> '#{cursor_x} …'` | per pane |
| pane modes (copy mode, …) | `#{pane_in_mode}`, `#{pane_mode}`, `%pane-mode-changed` | |
| terminal modes that affect input, e.g. bracketed paste, per-pane key reporting | pane application state | **not fully solved**: tracked in §5 |

A single `capture-pane` is *not* a terminal state snapshot. The sequence above is a
deliberate approximation whose limits are stated here rather than hidden.

## 2. The watermark rule

Everything the server sends to one control client goes through one ordered stream.
Therefore a `capture-pane` response is an exact watermark for that pane:

* `%output` written **before** the response is already contained in the captured
  screen — applying it again would duplicate text and corrupt full-screen programs;
* `%output` written **after** the response is not contained in the capture — it must
  be applied, in order.

The implementation:

1. a pane starts with `snapshot_loaded = false`; its `%output` is **dropped** and
   counted (`RemoteModel::dropped_output_bytes`);
2. when the `capture-pane` response is processed, the snapshot is handed to the
   caller as `SessionEvent::PaneSnapshot`, and the pane is marked ready;
3. every later `%output` for that pane is delivered as `SessionEvent::PaneOutput` in
   stream order.

This is the opposite of the tempting "replay everything received while connecting"
rule, which double-applies output. The rule is verified by `poc reconnect`, which
reads a tick counter painted by a full-screen program: after a 3-second detach the
restored tick must be **greater** than the value seen before detaching (not stale)
and **no larger** than elapsed time allows (not double-applied).

## 3. Fixed initialisation order

The order is part of the specification because the watermark depends on it:

1. `display-message -p <marker>` — the barrier. Everything the server sends before
   this response is the server's own initialisation output for commands we never
   issued; it is surfaced as `SessionEvent::Unsolicited` and never matched to a
   client request.
2. `list-sessions`
3. `list-windows -a`
4. `list-panes -a`
5. per pane: `capture-pane` (the watermark)
6. per pane: `display-message -p` metadata

Only one command is in flight at a time during the snapshot, so each response is
unambiguously attributable. Pipelining is a later optimisation and must not change
these semantics.

## 4. Failure handling

| Situation | Behaviour |
|---|---|
| a snapshot command returns `%error` | `SyncError::CommandFailed`; the session reports it, the model stays untrusted |
| a barrier marker does not match | `SyncError::Malformed`; nothing is applied |
| a list line cannot be parsed | `SyncError::Malformed`; nothing is applied |
| a layout string cannot be parsed | the model is marked `NeedsResync` and the raw string is kept |
| `%sessions-changed` (no payload) | the model is marked `NeedsResync`; a fresh query is required |
| parser desync (limits, framing) | `SessionEvent::Desync`; the model is marked `NeedsResync` |
| `%exit` or transport failure | the queue is closed, outstanding requests fail with `closed_by_exit`, writes stop permanently |
| `pause`/`continue` and dropped output | dropped bytes are counted and reported; the UI must be able to show "resynchronised" separately from "history continuity guaranteed" |

"No silent wrong screen" is the rule: a client that cannot prove continuity must
say so.

## 5. Known gaps (tracked, not hidden)

* **Wrapped logical lines.**  The capture is row-exact: `-J` (join wrapped lines)
  is *not* used, because joining turns physical rows into logical lines and makes
  "the last `rows` lines are the visible screen" false — which is exactly the split
  the watermark depends on.  Observed consequence on tmux 3.4: a 78-row pane reports
  76 lines with `-J` and 78 without.  The price is that a wrapped line is restored
  as two physical rows without the wrap flag; the screen is identical, a later
  re-wrap on resize may not be.  Verified by `poc core-gate`, whose text comparison
  is cell-for-cell against a second `capture-pane` of the same pane.

* **Application modes across a reconnect.** Bracketed paste, application cursor
  keys, mouse reporting and per-pane key reporting live inside the pane's
  application; `capture-pane` cannot reproduce them, and tmux only lets a control
  client submit terminal reports on newer versions. Until this is verified per
  supported version, a reconnect must be documented as *screen-accurate* but not
  guaranteed *mode-accurate* — an editor may need a nudge (redraw) after a
  reconnect. This is exactly the class of problem the plan calls out.
* **History continuity.** The client knows how many lines the server still has
  (`#{history_size}` / `#{history_limit}`) but cannot recover lines the server has
  already discarded, and must not claim to.
* **Alternate screen.** The capture is taken from whatever screen is active;
  `#{alternate_on}` says which one. Rebuilding a *switchable* pair of screens needs
  the terminal emulator to accept a restore that does not re-emit the original
  switching sequences.

## 6. Verification plan

| Claim | Verified by |
|---|---|
| watermark rule | `poc reconnect` tick assertions |
| snapshot restores the grid, cursor, alternate screen and scrollback | `poc core-gate` (cell-by-cell against a second `capture-pane`, plus tmux's own cursor/alternate/history values) |
| a restore is silent and responses are never written back | `crates/term-core/tests/gate.rs` gate 3 |
| layout fidelity (3 panes, external split) | `poc reconnect` layout assertions |
| external changes reflected without echoing them back | `poc reconnect` (split from a second client) |
| input still works after restore | `poc reconnect` typed-command check |
| no control command leaks to the shell | `poc leak`, plus `poc leak --inject-bug` as a falsification test |
| byte-level parser robustness | `crates/tmux-protocol/tests/boundary.rs` (every split point of every fixture) |
| limit handling | `Parser` unit tests + `boundary.rs` truncation cases |