# Evidence

Black-box captures are the factual basis for `docs/spec/control-mode.md`. They are
produced by one script, with the command sequence and the tool versions recorded
next to the bytes, so that any claim can be re-derived rather than trusted.

## Layout

```
crates/tmux-protocol/tests/fixtures/
├── manifest.json                     tmux version, timestamp, probe description
├── probe_control_mode.py             a copy of the exact script that ran
├── attach_and_commands.*.bin         raw byte captures, one file per step
├── notifications.*.bin
├── cc_entry.*.bin
└── plain_shell_leak.*.bin
```

The current corpus was recorded from **tmux 3.4** (see `manifest.json` for the
exact version, time and command sequence); it is 40 captures covering attach,
command framing, errors, binary and CJK pane output, every structural
notification, layout strings, the `-CC` takeover on a PTY and a plain-shell
control.  It was re-recorded after the probe fixes: copy mode is now entered with
the `copy-mode` command (so `notifications.13/14` contain a real
`%pane-mode-changed`, in tmux 3.4's bare form), and
`attach_and_commands.12_percent_inside_block` now contains a response line that
starts with `%`.

The captures are committed on purpose: they are small, they are the evidence, and
they let the parser tests run without a tmux server.

## Reproducing

```bash
tools/container/run.sh python3 tools/probe/probe_control_mode.py \
    crates/tmux-protocol/tests/fixtures
```

The script uses a **private** socket (`-L muxterm-probe-<pid>`) and kills its own
server; it never touches a running tmux.

## Using the captures in tests

* `tests/fixtures.rs` replays the captures through the parser and asserts the
  invariants: response framing and numbers, `%error` blocks, binary/CJK output,
  layout orientation, `%pane-mode-changed` in its recorded bare form, a
  `%`-prefixed line that must stay inside a response block, and a format-expanded
  window name.  It does not pin every capture's complete event sequence; the
  boundary test does pin the stream under every split.
* `tests/boundary.rs` re-runs the same replay while feeding the capture **one byte
  at a time** and with every possible single split point, asserting that the event
  stream is identical. A protocol parser that only works when reads align with line
  boundaries is not a protocol parser.

## Rules

* A capture is never edited by hand. If behaviour changes, record a new capture and
  update the specification; do not "fix" the bytes.
* A capture that is no longer used by a test is deleted, not kept as decoration.
* Captures contain only what a local tmux emitted into a private socket. They are
  checked for host paths and usernames before being committed.