# Third-party dependencies and licences

Policy: every dependency is reviewed before it enters the build, and the review is
recorded here. Dependencies whose licence is incompatible with MIT distribution
are not used.

## Runtime dependencies

| Crate | Version | Licence | Why | Review |
|---|---|---|---|---|
| — (`tmux-protocol`) | — | — | intentionally **zero** dependencies: replay, property and fuzz testing must not need a runtime | — |
| — (`tmux-client`) | — | — | standard library only; process spawning and threads are enough for the protocol phase | — |
| `alacritty_terminal` | 0.26 | Apache-2.0 | terminal emulation behind our own `TerminalCore` boundary; adopted after the gate in `docs/adr/0002`, decision recorded in `docs/adr/0004` | permissive; comes with a tty/event-loop half we do not use, reviewed and accepted |

## Development / PoC dependencies

| Crate | Version | Licence | Why | Review |
|---|---|---|---|---|
| `portable-pty` | 0.8 | MIT | cross-platform PTY for the takeover (`-CC`) scenario, and the intended local-terminal backend later; it is the abstraction the plan selects | permissive, no copyleft |

## External programs (not linked, invoked as processes)

| Program | Licence | Use |
|---|---|---|
| tmux | ISC | the system under test; never modified, never vendored |
| OpenSSH client | BSD-style | SSH transport for remote tmux |
| python3 | PSF | runs the fixture probe script and the mouse probe used by `poc mouse`; POSIX `sh` runs the application-mode probe used by `poc modes` |

## Planned but not yet adopted

These are candidates from the plan; they are added only with a recorded review:

* windowing: `winit` (Apache-2.0/MIT);
* rendering: `wgpu` (Apache-2.0/MIT);
* text shaping: `cosmic-text` / `swash` (Apache-2.0/MIT);
* peripheral GUI: `egui` (Apache-2.0/MIT).

## What we do not use

* iTerm2 source code, resources or derived design documents (GPLv2) — see
  `docs/clean-room.md`;
* copyleft libraries in the shipped binary.