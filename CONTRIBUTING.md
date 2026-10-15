# Contributing

## Before anything else

Read [`docs/clean-room.md`](docs/clean-room.md). The short version: this project is
an independent implementation. Do not copy, translate or paste another terminal's
source code, and do not commit code produced by feeding such source to a tool.

## Every contribution states its provenance

Add a line to your pull request (and to the commit trailer) describing where the
behaviour you implemented comes from:

```
Provenance: tmux(1) CONTROL MODE, section "control mode commands"
Provenance: black-box capture tests/fixtures/attach_and_commands.07_cmd_binary_output.bin
Provenance: our own specification docs/spec/state-sync.md §2
Provenance: no external source; design decision (see docs/adr/0003-…)
```

If you have read the reference implementation (iTerm2) in the past, say so. An
honest note is worth more than a claim nobody can check.

## Rules the code follows

1. **No I/O in `tmux-protocol`.** It parses bytes, encodes commands and schedules
   them. If it needs a socket, it belongs in `tmux-client`.
2. **Bytes are not strings.** Pane output, names and titles stay `Vec<u8>` until
   something displays them.
3. **No silent repair.** Desynchronisation, truncation, dropped output and refused
   commands are reported through a type or an event. A test that hides one of these
   is a bug.
4. **Bounds on everything a peer can grow.** See `Limits`.
5. **Two quoting layers stay separate.** tmux arguments use
   `tmux_protocol::command::quote_arg`; remote shell commands use
   `tmux_client::transport::shell_quote`. Never one helper for both.
6. **The remote state is the source of truth.** The GUI projects it; it does not
   edit it locally and hope the server agrees.

## Running things

```bash
tools/container/test.sh          # fmt, clippy, unit tests, fixture replay
tools/container/e2e.sh           # the PoC scenarios against a real tmux
tools/container/run.sh bash      # interactive
```

Nothing is built or tested on the host — see
[`docs/dev-environment.md`](docs/dev-environment.md) for why.

## Adding a protocol behaviour

1. Record a capture: `tools/container/run.sh python3 tools/probe/probe_control_mode.py crates/tmux-protocol/tests/fixtures`.
2. Write the rule into `docs/spec/control-mode.md` with its provenance marker.
3. Add a parser event if needed, with a unit test that fails before the change.
4. Add or extend a fixture assertion so the behaviour is pinned to real bytes.

## Commits

Small, single-purpose commits. A protocol change and a UI change are two commits.
The specification change goes in the same commit as the code that implements it —
otherwise the specification is fiction.