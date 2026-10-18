# Muxterm

**A cross-platform terminal where tmux windows are tabs and tmux panes are
splits.** Muxterm drives tmux through its public *control mode* protocol, so the
remote session's windows and panes become native GUI objects instead of a
screen-scraped TUI.

> Status: **protocol, synchronisation and terminal core are done and verified
> against a real tmux 3.4.** A headless driver proves the things everything else
> depends on: a reliable channel, correct state restoration after a reconnect, a
> live resynchronisation for panes that appear later, no control command ever
> reaching a user's shell, a size announcement that actually resizes the remote
> window, byte-exact mouse input, and a terminal core that rebuilds what tmux says
> a pane shows and reports what changed. The GUI comes next; there is none yet, on
> purpose.

## What is implemented

| Crate | Purpose | State |
|---|---|---|
| `crates/tmux-protocol` | byte-level control-mode parser, command encoder, command queue, `-CC` takeover switch, layout parser. **No I/O, no GUI, no dependencies.** | implemented; unit tests + 40 recorded captures replayed under every byte split |
| `crates/tmux-client` | transports (local `tmux`, `ssh`), connection epochs, remote object model, snapshot/resync/incremental-capture engine, flow-control and size commands | implemented; unit tests plus scripted-transport session tests |
| `crates/term-core` | terminal-emulation boundary: project-owned `TerminalCore` trait, read-only view types, idempotent snapshot restore, damage reporting, mouse/paste/focus encoders, side-effect isolation | implemented on `alacritty_terminal` 0.26 after passing the gate in `docs/adr/0002` |
| `crates/poc-cli` | headless driver: `create`, `snapshot`, `stream`, `resize`, `mouse`, `reconnect`, `leak`, `core-gate`, `doctor` | implemented; all scenarios pass against a real tmux |

Not started yet: renderer, GUI shell, tabs/splits UI, IME, settings, packaging.

The design decisions and their limits are recorded in `docs/adr/`: the workspace
boundaries, the terminal-core gate, the watermark and barrier, the core choice,
and resynchronisation/incremental capture/refresh (`docs/adr/0005`).

## Build and test

**Locally everything runs inside the project container.** No tmux server, PTY or
cargo artefact is created on the developer's machine, so a test run cannot touch
the desktop session, the user's own tmux server, or `/tmp`. CI runs the same
commands on a clean Ubuntu 24.04 runner (`tmux 3.4`).

```bash
tools/container/build-image.sh      # once (needs network for the base image)
tools/container/test.sh             # fmt + clippy + unit/integration tests + fixtures
tools/container/run.sh bash         # interactive shell inside the sandbox
```

Requires Rust **1.85** (the MSRV of `alacritty_terminal` 0.26); CI checks both the
MSRV and current stable.

The run sees only this repository, gets a private `/tmp` (so `tmux -L …` sockets
cannot collide with anything on the host), and keeps its cargo caches in named
volumes. See [`docs/dev-environment.md`](docs/dev-environment.md).

### The PoC scenarios

```bash
tools/container/e2e.sh   # create, snapshot, stream, resize, mouse, modes, reconnect, core-gate, leak
```

or step by step:

```bash
tools/container/run.sh bash -lc '
  cargo build -p poc-cli &&
  ./target/debug/poc create    --socket poc &&
  ./target/debug/poc snapshot  --socket poc &&
  ./target/debug/poc resize    --socket poc &&
  ./target/debug/poc mouse     --socket poc &&
  ./target/debug/poc modes     --socket poc &&
  ./target/debug/poc reconnect --socket poc --detach-seconds 3 &&
  ./target/debug/poc core-gate --socket poc &&
  ./target/debug/poc leak      --socket poc &&
  ./target/debug/poc leak      --socket poc --inject-bug   # must FAIL
'
```

`reconnect` proves that a reattach restores a *current* screen and that live
output continues in order.  `core-gate` proves that the terminal core rebuilds what
tmux says a pane shows — text cell by cell, plus cursor, alternate screen and
scrollback — and that restore is idempotent.  `resize` announces a new client size
and checks that tmux and the model agree.  `mouse` sends an SGR report through
`send-keys -H` and reads it back from a pane application byte-for-byte.  `modes`
reattaches to a pane application that asked for SGR mouse reports, application
cursor keys and bracketed paste, and checks that the modes tmux reports survive
the reconnect and reach the input encoder.  `leak`
runs `tmux -CC` inside a real shell on a real PTY and proves that no control
command reaches the shell after control mode ends; `--inject-bug` deliberately
breaks that rule and the same detector must catch it.

## Documentation

* [`docs/clean-room.md`](docs/clean-room.md) — what may and may not enter the
  implementation, and the evidence we keep.
* [`docs/spec/control-mode.md`](docs/spec/control-mode.md) — the protocol
  specification this implementation follows.
* [`docs/spec/state-sync.md`](docs/spec/state-sync.md) — the watermark,
  resynchronisation and incremental pane capture.
* [`docs/spec/encoding.md`](docs/spec/encoding.md) — tmux, SSH and terminal input
  quoting layers.
* [`docs/spec/input.md`](docs/spec/input.md) — keys, paste, mouse and focus.
* [`docs/spec/compatibility.md`](docs/spec/compatibility.md) — supported tmux
  versions and how each claim is tested.
* [`docs/evidence/`](docs/evidence/README.md) — recorded black-box captures and
  their provenance.
* [`docs/adr/`](docs/adr/) — design decisions, including the resync/refresh design.

## Licence

MIT — see [`LICENSE`](LICENSE). Third-party components keep their own licences; the
dependency policy is in [`docs/licences.md`](docs/licences.md).

This repository contains **no iTerm2 code, resources or derived design
documents**; how compatibility work is kept independent is described in
[`docs/clean-room.md`](docs/clean-room.md).