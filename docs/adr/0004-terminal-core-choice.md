# ADR 0004: terminal core — `alacritty_terminal` 0.26

* Status: accepted
* Provenance: third-party dependency, Apache-2.0 (see `docs/licences.md`).  The
  gate it had to pass is our own (`docs/adr/0002`); the verification is ours.

## Decision

`alacritty_terminal` 0.26 backs `crates/term-core`, behind the project-owned
`TerminalCore` trait.  Nothing above `term-core` names an emulator type: the view
types (`ScreenView`, `CellView`, `Color`, `Modes`, `Cursor`) are ours.

## What it does well

| Gate | Result |
|---|---|
| 1. rebuild the grid from a capture | text compares **cell for cell** with `capture-pane` for every pane, including styled runs, CJK wide characters, a wrapped line and 64 lines of scrollback |
| 2. state from outside the stream | cursor (via `Handler::goto`), alternate screen (`?1049h/l` through the byte interface), scroll region, and the history bound are all settable; the restored cursor and alternate flag match tmux's report exactly |
| 3. side-effect isolation | the `EventListener` gives `PtyWrite`, `Bell`, `ClipboardStore/Load`, `Title`, `ColorRequest`; we capture or count all of them and never write back.  A restore runs with suppression on, so replaying a screen cannot answer an old query |
| 4. read-only export | `Term::grid()`/`renderable_content()` are `&self`; the trait is object safe, so a renderer takes `&dyn TerminalCore` and cannot mutate the terminal.  The core owns no window, GPU or event-loop state we use |
| 5. bounded memory | `Config::scrolling_history` on construction and `Term::set_options` at runtime; measured: 50 lines fed into a 5-line bound leaves 5, and lowering the bound trims what is already there |

Recorded timings from `poc core-gate` on tmux 3.4 (container): restoring and
cross-checking three panes, one of them with 64 history lines, completes in well
under a second.

## What it does not do, and how we live with it

* **The wrap flag of a joined line is not restored.**  The capture is row-exact
  (no `-J`), so a logical line that wrapped is restored as two physical rows.  The
  screen looks identical; a later re-wrap on resize may not.  Documented in
  `docs/spec/state-sync.md` §5.
* **Per-pane application modes** (bracketed paste, application cursor keys, mouse
  reporting) live in the remote application and are not derivable from a capture.
  The core can *report* its own modes, but a reconnect cannot set the pane's.  Same
  section.
* **Erase-display pushes a line into scrollback** in this core, which would add
  phantom history to every restore.  The adapter avoids `CSI 2 J` and clears rows
  individually instead; the reason is recorded in the code next to the call.
* **Cost**: the crate pulls in its tty/event-loop machinery (rustix, signal-hook,
  …) which we do not use.  Accepted for now; if the dependency weight becomes a
  problem, the boundary makes a narrower fork or a different core a contained
  change.

## Alternatives considered

* **`vte` alone** — rejected in `docs/adr/0002`: it is a parser, not an emulator,
  and completing it would turn the project into a terminal-compatibility effort.
* **Rendering tmux's TUI output as text** — rejected: it cannot express tabs or
  splits as native GUI objects, which is the product.
