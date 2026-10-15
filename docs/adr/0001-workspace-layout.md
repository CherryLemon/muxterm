# ADR 0001: workspace layout and the I/O boundary

* Status: accepted
* Date: recorded with the first PoC commit
* Provenance: our own design decision, guided by the public tmux control-mode
  protocol description.

## Context

The protocol work must be testable without a terminal, a server or a display, and
the parts that talk to the outside world must be replaceable. At the same time the
product needs SSH, PTYs, PTY-less control channels and, eventually, a GPU renderer.

## Decision

A Cargo workspace with hard boundaries:

```
crates/tmux-protocol   bytes <-> events and commands.  No I/O, no GUI, no deps.
crates/tmux-client     transports, connection epochs, remote model, snapshot.
crates/poc-cli         headless driver for the PoC scenarios.
```

Rules:

1. `tmux-protocol` may not depend on anything outside the standard library. This is
   what makes replay tests, property tests and fuzzing trivial, and it is why the
   parser can be exercised against recorded byte captures in CI without a tmux.
2. `tmux-protocol` may not open a socket or spawn a process. Command *encoding* is
   its job; command *delivery* is not.
3. `tmux-client` owns everything with a side effect, behind the `Transport` trait,
   so tests can substitute a scripted transport.
4. The GUI will be a separate crate that consumes `SessionEvent`s. It never touches
   the connection directly.

## Consequences

* The parser and encoder test suites are fast and deterministic.
* A scripted transport makes session-level tests possible without tmux.
* Slightly more plumbing: every new capability has to be placed deliberately on one
  side of the boundary.
* Future async I/O can be introduced inside `tmux-client` without touching the
  protocol crate.