# Compatibility matrix

A claim only counts when it names the version **and** the test that supports it.
The matrix starts with three tmux releases: an older deployment, a middle one, and
the newest reviewed release.

| tmux | Why it is in the matrix | Status |
|---|---|---|
| 3.3a | oldest supported; Debian bookworm's `tmux` | to be built into the image, tests pending |
| 3.4 | the version actually available on the development host and in the test image | **exercised**: 40 recorded captures, the full unit suite and both PoC scenarios pass against it |
| 3.5a | middle ground, widely deployed | to be built into the image, tests pending |
| 3.8 | current release at the time of writing | to be built into the image, tests pending |

Versions are selected with `PATH=/opt/tmux/<version>/bin:$PATH`; see
`tools/container/build-tmux-versions.sh`.

The captures in `crates/tmux-protocol/tests/fixtures/manifest.json` record which
version produced them; a capture that no longer matches a supported release is a
test failure, not a footnote.

## Capability detection, not version comparison

Feature availability is probed from behaviour, never inferred from a version
string:

* `%pane-mode-changed` with and without a mode name — the parser accepts both, so
  no probe is needed to read the stream; a probe is needed only if the UI wants to
  display the mode name on old releases;
* `%extended-output` only appears with flow control (`refresh-client -A`); the
  parser handles its absence by construction;
* `send-keys -H` is used for byte-exact input and is the first thing a new tmux
  version must be tested against;
* `capture-pane -e -q -J -S - -E -` options are all long-standing, but the exact
  output shape is a per-version fixture test rather than an assumption.

## What is tested per version

| Area | Test | Kind |
|---|---|---|
| protocol parsing | `cargo test -p tmux-protocol` | unit + fixture replay |
| arbitrary byte splits | `cargo test -p tmux-protocol --test boundary` | property test over every fixture |
| snapshot & restore | `poc reconnect` | black-box, real server |
| takeover / no leakage | `poc leak`, `poc leak --inject-bug` | black-box, real PTY |
| layout orientations | fixture test asserting `[` vs `{` against a real split | black-box |
| terminal-core restore fidelity | `poc core-gate` (grid vs `capture-pane`, cursor, alternate screen, scrollback) | black-box, real server |

## Platform coverage (target, not yet claimed)

| Client platform | Local terminal | tmux backend |
|---|---|---|
| macOS | POSIX PTY → shell | local tmux or SSH remote tmux |
| Linux | POSIX PTY → shell | local tmux or SSH remote tmux |
| Windows | ConPTY → PowerShell, cmd | tmux inside WSL, or SSH remote tmux |

Windows notes: upstream tmux does not run natively there, and a control protocol
must not be assumed to survive a ConPTY hop byte-for-byte. The supported Windows
paths are the two above; "arbitrary SSH launched from an arbitrary PowerShell
session, then `tmux -CC`" is **not** claimed until it is verified in that shape.