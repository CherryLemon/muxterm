//! The terminal-emulation boundary.
//!
//! Everything above this crate (rendering, tabs, splits, the GUI) talks to
//! [`TerminalCore`] and to the project-owned view types in [`view`].  The concrete
//! emulator is an implementation detail, so replacing it stays possible.
//!
//! The crate exists because of the capability gate in `docs/adr/0002`: a candidate
//! is only adopted if it can
//!
//! 1. **rebuild a grid from a tmux snapshot** (`capture-pane` output plus per-pane
//!    metadata), including styles and wide characters;
//! 2. accept cursor, alternate-screen, history and scroll-region **state from
//!    outside the byte stream**;
//! 3. isolate side effects: terminal responses are *captured*, never written back,
//!    and nothing rings, beeps or touches the clipboard while a snapshot is
//!    restored;
//! 4. export a **read-only** view a renderer can consume without mutable access;
//! 5. enforce a configurable **scrollback bound**.
//!
//! `docs/adr/0004-terminal-core-choice.md` records how `alacritty_terminal` was
//! measured against those five points and where it falls short.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

pub mod alacritty_core;
pub mod snapshot;
pub mod view;

pub use alacritty_core::{AlacrittyCore, CoreListener, SideEffectCounts};
pub use snapshot::{RestoreError, RestoreReport, ScreenSnapshot};
pub use view::{CellFlags, CellView, Color, Cursor, CursorShape, LineView, Modes, ScreenView};

/// The boundary the rest of the product depends on.
///
/// Implementations must be usable from a single-threaded event loop: no method
/// takes a lock the caller cannot see, and reading the screen never blocks input.
pub trait TerminalCore {
    /// Feeds live pane bytes (`%output` payloads).
    fn feed(&mut self, bytes: &[u8]);

    /// Resizes the grid.
    fn resize(&mut self, columns: u16, rows: u16);

    /// Rebuilds the screen from a tmux snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`RestoreError`] when the snapshot does not fit the core (for
    /// example a zero-sized pane).  A failed restore leaves the core usable but
    /// must not be presented as accurate.
    fn restore(&mut self, snapshot: &ScreenSnapshot) -> Result<RestoreReport, RestoreError>;

    /// Read-only view of the current screen.
    fn screen(&self) -> ScreenView<'_>;

    /// Current cursor position and shape.
    fn cursor(&self) -> Cursor;

    /// Terminal modes that affect how input must be encoded.
    fn modes(&self) -> Modes;

    /// Lines currently held in scrollback.
    fn history_size(&self) -> usize;

    /// Changes the scrollback bound; must take effect on the next write.
    fn set_history_limit(&mut self, lines: usize);

    /// Takes the responses the emulator generated (device status reports, device
    /// attributes, cursor position reports, ...).
    ///
    /// They are never written anywhere by this crate: a tmux control client must
    /// decide per pane whether a report is wanted, and during a snapshot restore
    /// the answer is always no.
    fn take_responses(&mut self) -> Vec<Vec<u8>>;

    /// Counters of side effects the emulator asked for but this crate did not
    /// perform (bell, clipboard, title changes).
    fn side_effects(&self) -> SideEffectCounts;
}
