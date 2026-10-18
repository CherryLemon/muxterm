# Compatibility matrix

A claim only counts when it names the version **and** the test that supports it.
The matrix starts with four tmux releases: an older deployment, a middle one, and
the newest reviewed releases.

| tmux | How it is obtained | Status |
|---|---|---|
| 3.3a | Debian bookworm's `tmux` package (the default Dockerfile base); also buildable with `tools/container/build-tmux-versions.sh 3.3a` | supported baseline; **exercised**: `poc snapshot`, `resize`, `mouse`, `modes`, `reconnect`, `core-gate` and `leak` pass against a source build |
| 3.4 | built from the public release tarball into `/opt/tmux/3.4` by the test image, and first in `PATH` | **exercised**: 40 recorded captures, the full unit suite, `poc resize`, `poc mouse`, `poc modes`, `poc reconnect`, `poc core-gate`, `poc leak` |
| 3.5a | `tools/container/build-tmux-versions.sh 3.5a` | buildable; tests pending |
| 3.8 | `tools/container/build-tmux-versions.sh 3.8` | **exercised**: the same scenario set as 3.3a/3.4 passes; the one observable difference is `#{bracket_paste_flag}`, present from 3.7, so `poc modes` restores bracketed paste there and reports it unknown on older releases |

Versions are selected with `PATH=/opt/tmux/<version>/bin:$PATH`; see
`tools/container/build-tmux-versions.sh`. The script is a plain
`configure && make` build of the upstream release tarball; it does not patch tmux.

The captures in `crates/tmux-protocol/tests/fixtures/manifest.json` record which
version produced them (tmux 3.4); a capture that no longer matches a supported
release is a test failure, not a footnote. The corpus was re-recorded with the
fixed probe: `notifications.13/14_copy_mode` now contain a real
`%pane-mode-changed` (tmux 3.4 sends the **bare** form on enter and leave), and
`attach_and_commands.12_percent_inside_block` now contains a response line that
actually starts with `%`.

## Capability detection, not version comparison

Feature availability is probed from behaviour, never inferred from a version
string:

* `%pane-mode-changed` with and without a mode name — the parser accepts both; tmux
  3.4's bare form is recorded, and the model treats "no name" as a toggle. No probe
  is needed to read the stream; a probe is needed only if the UI wants to display
  the mode name on old releases;
* `%extended-output` only appears with flow control (`refresh-client -A`); the
  parser handles its absence by construction;
* `send-keys -H` is used for byte-exact input and is the first thing a new tmux
  version must be tested against;
* the per-pane application mode flags are probed by their own expansion, not by
  `tmux -V`: `#{mouse_any_flag}`, `#{mouse_*_flag}`, `#{keypad*_flag}`,
  `#{cursor,insert,origin,wrap}_flag` are present on 3.3a and later, while
  `#{bracket_paste_flag}` expands to the empty string before 3.7. An empty field is
  parsed as **unknown** (`None`), so an old server makes the client omit the mode
  instead of guessing that it is off (`poc modes` checks exactly this);
* `capture-pane -e -q -N -S <history> -E -` options are all long-standing; the
  exact output shape is a per-version fixture test rather than an assumption. `-J`
  is never used (it would destroy the history/screen split), and the history bound
  is derived from `Limits::max_response_lines`.

## What is tested per version

| Area | Test | Kind |
|---|---|---|
| protocol parsing | `cargo test -p tmux-protocol` | unit + fixture replay |
| arbitrary byte splits | `cargo test -p tmux-protocol --test boundary` | property test over every fixture |
| snapshot & restore | `poc reconnect` | black-box, real server |
| incremental pane capture / resync | `crates/tmux-client/tests/session.rs` | scripted transport |
| client size / screen refresh | `poc resize` | black-box, real server |
| mouse encoding and delivery | `poc mouse` | black-box, real server + pane application |
| per-pane application modes across a reconnect | `poc modes` | black-box, real server + pane application |
| takeover / no leakage | `poc leak`, `poc leak --inject-bug` | black-box, real PTY |
| layout orientations | fixture test asserting `[` vs `{` against a real split | black-box |
| terminal-core restore fidelity and idempotence | `poc core-gate` + `crates/term-core/tests/gate.rs` | black-box + synthetic |

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