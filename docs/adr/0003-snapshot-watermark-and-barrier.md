# ADR 0003: snapshot watermark and the content-verified barrier

* Status: accepted
* Provenance: our own design decision, resting on two properties of the public
  control-mode protocol (single ordered stream per client; the server never injects
  notifications inside a response block). Verified by `poc reconnect` and by the
  recorded captures.

## Context

Two problems have to be solved before a GUI can trust anything it draws.

1. **Restoring state after a reconnect.** The obvious recipe — take a
   `capture-pane` snapshot, then replay every `%output` received during the connect
   — is wrong in both directions: it re-applies output that the snapshot already
   contains, and it can miss the bytes that were in flight while the capture ran.
2. **Attributing the first responses.** A control client cannot know how many
   commands the tmux client issues while attaching, and its own counter is not the
   server's. The first `%begin` block a client sees may therefore answer something
   it never sent.

## Decision

**Watermark.** A `capture-pane` response is treated as an exact watermark for that
pane, because everything a control client receives goes through one ordered stream:

* while a pane's snapshot is not loaded, its `%output` is dropped and counted;
* the moment the capture response is processed, the snapshot is handed to the
  emulator and the pane becomes live;
* later `%output` is applied in stream order.

Dropping is deliberate and observable (`RemoteModel::dropped_output_bytes`), and
the rule is verified by a full-screen program that paints a tick counter: after a
detach the restored tick must be newer than the last tick seen before the detach and
no newer than elapsed time allows.

**Barrier.** The first command a client sends after attaching is a marker
(`display-message -p <nonce>`), and it is registered with the queue as a
*content-verified* barrier: while it is outstanding, a response block is matched to
it only if its first line is the marker. Any other block is surfaced as unsolicited
initialisation output, and the marker request keeps its place in flight.

## Consequences

* Reconnect correctness does not depend on guessing how many blocks to skip, or on
  the timing of the server's own attach bookkeeping.
* Cost: one extra round trip at connect time, and one command in flight during the
  snapshot. Pipelining later must preserve the watermark semantics, not merely the
  command order.
* A snapshot is screen-accurate, and mode-accurate only where the server exposes
  the mode: the per-pane metadata reports the mouse protocol, application cursor
  keys, the keypad, cursor/insert/origin/wrap and — from tmux 3.7 — bracketed paste,
  and those are restored. Focus reporting and UTF-8 mouse coordinates are not
  recoverable. That limit is documented in `docs/spec/state-sync.md` §5 instead of
  being hidden.
