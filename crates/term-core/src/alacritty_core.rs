//! Adapter for the candidate terminal core.
//!
//! The emulator is used through its **public byte interface** wherever possible
//! (feed the captured escape sequences), and through its public state API only for
//! the things a capture cannot express: cursor position, alternate screen, history
//! bound and scroll region.
//!
//! Two properties matter more than anything else here:
//!
//! * **nothing is written back**.  Every response the emulator wants to send goes
//!   to a listener that only counts and stores it ([`SideEffectCounts`],
//!   [`AlacrittyCore::take_responses`]).
//! * **a restore is quiet**.  While a snapshot is painted, responses are dropped
//!   entirely and counted, so replaying a screen can never answer a query that was
//!   asked before the disconnect.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Handler, Processor};

use crate::snapshot::{InputModes, RestoreError, RestoreReport, ScreenSnapshot};
use crate::view::{Cursor, Modes, ScreenView};
use crate::{Damage, DamageLine, TerminalCore};

/// Counters for everything the emulator asked for and this crate refused to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideEffectCounts {
    /// Bell requests.
    pub bell: u64,
    /// Clipboard writes requested by the application (OSC 52).
    pub clipboard_store: u64,
    /// Clipboard reads requested by the application.
    pub clipboard_load: u64,
    /// Window-title changes.
    pub title: u64,
    /// Colour queries.
    pub color_request: u64,
    /// Terminal size queries.
    pub size_request: u64,
    /// Anything else the emulator reported.
    pub other: u64,
    /// Responses dropped because a restore was in progress.
    pub responses_suppressed: u64,
}

#[derive(Debug, Default)]
struct ListenerState {
    responses: Vec<Vec<u8>>,
    effects: SideEffectCounts,
    suppress: bool,
}

/// Shared state of the event listener, readable from outside the terminal.
#[derive(Debug, Clone, Default)]
pub struct CoreListener {
    state: Arc<Mutex<ListenerState>>,
}

impl CoreListener {
    /// Creates a listener that captures everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with_state<R>(&self, f: impl FnOnce(&mut ListenerState) -> R) -> R {
        // A poisoned lock only means some other thread panicked while counting; the
        // counters are still consistent enough to keep using.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f(&mut state)
    }

    /// Takes the responses generated since the last call.
    pub fn take_responses(&self) -> Vec<Vec<u8>> {
        self.with_state(|state| std::mem::take(&mut state.responses))
    }

    /// The side-effect counters.
    #[must_use]
    pub fn effects(&self) -> SideEffectCounts {
        self.with_state(|state| state.effects)
    }

    /// Suppresses (and counts) responses until the returned guard is dropped.
    fn suppress(&self) -> Suppression<'_> {
        self.set_suppress(true);
        Suppression { listener: self }
    }

    fn set_suppress(&self, value: bool) {
        self.with_state(|state| state.suppress = value);
    }

    fn record(&self, event: Event) {
        self.with_state(|state| match event {
            Event::PtyWrite(text) => {
                if state.suppress {
                    state.effects.responses_suppressed += 1;
                } else {
                    state.responses.push(text.into_bytes());
                }
            }
            Event::Bell => state.effects.bell += 1,
            Event::ClipboardStore(..) => state.effects.clipboard_store += 1,
            Event::ClipboardLoad(..) => state.effects.clipboard_load += 1,
            Event::Title(_) | Event::ResetTitle => state.effects.title += 1,
            Event::ColorRequest(..) => state.effects.color_request += 1,
            Event::TextAreaSizeRequest(_) => state.effects.size_request += 1,
            // Redraw hints for a GUI, not requests to the outside world.  A restore
            // that switches on mouse reporting provokes `MouseCursorDirty`, and
            // counting that as a refused side effect would make every reattach look
            // like it tried to do something.
            Event::MouseCursorDirty | Event::CursorBlinkingChange => {}
            _ => state.effects.other += 1,
        });
    }
}

struct Suppression<'a> {
    listener: &'a CoreListener,
}

impl Drop for Suppression<'_> {
    fn drop(&mut self) {
        self.listener.set_suppress(false);
    }
}

/// The handle the emulator owns; it only forwards to [`CoreListener`].
#[derive(Debug, Clone)]
pub struct ListenerHandle(CoreListener);

impl EventListener for ListenerHandle {
    fn send_event(&self, event: Event) {
        self.0.record(event);
    }
}

/// Grid geometry handed to the emulator.
#[derive(Debug, Clone, Copy)]
struct Size {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// The terminal core, backed by `alacritty_terminal`.
pub struct AlacrittyCore {
    term: Term<ListenerHandle>,
    processor: Processor,
    listener: CoreListener,
    config: Config,
    columns: u16,
    rows: u16,
    /// Whether an operation that can change the viewport happened since the last
    /// damage report.  The emulator always reports the cursor cell as damaged,
    /// even when nothing moved; this lets a repeated `take_damage` say "nothing
    /// changed" instead of returning a one-cell report on every poll.
    damage_pending: bool,
    /// True until the first damage report or until the geometry/state is set from
    /// outside (resize, restore).  The emulator starts fully damaged; if live input
    /// arrives before the first report, the initial state is folded into an
    /// all-rows partial report so the caller still receives line ranges.
    initial_full: bool,
    /// Whether live input arrived while `initial_full` was still set.
    fed_since_start: bool,
}

impl AlacrittyCore {
    /// Creates a core with the given geometry and scrollback bound.
    ///
    /// A zero geometry is clamped to one cell so the emulator stays constructible;
    /// [`TerminalCore::restore`] rejects unusable snapshots instead.
    #[must_use]
    pub fn new(columns: u16, rows: u16, history_limit: usize) -> Self {
        let columns = columns.max(1);
        let rows = rows.max(1);
        let listener = CoreListener::new();
        let config = Config {
            scrolling_history: history_limit,
            ..Config::default()
        };
        let size = Size {
            columns: usize::from(columns),
            screen_lines: usize::from(rows),
        };
        let term = Term::new(config.clone(), &size, ListenerHandle(listener.clone()));
        Self {
            term,
            processor: Processor::new(),
            listener,
            config,
            columns,
            rows,
            damage_pending: true,
            initial_full: true,
            fed_since_start: false,
        }
    }

    /// The core's current geometry.
    #[must_use]
    pub const fn size(&self) -> (u16, u16) {
        (self.columns, self.rows)
    }

    /// Feeds bytes through the emulator's parser.
    fn feed_bytes(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
        self.damage_pending = true;
        if self.initial_full {
            self.fed_since_start = true;
        }
    }

    /// Feeds an ASCII control sequence written by this adapter.
    fn feed_control(&mut self, sequence: &str) {
        self.feed_bytes(sequence.as_bytes());
    }

    /// Moves the cursor explicitly, outside the byte stream.
    fn set_cursor(&mut self, column: u16, row: u16) {
        self.term.goto(i32::from(row), usize::from(column));
    }

    /// Switches on the application input modes a snapshot reported.
    ///
    /// Only *on* switches are emitted: the restore starts from RIS, so everything
    /// the snapshot did not ask for is already at the emulator default.  That also
    /// keeps the function idempotent, which a restore has to be.
    ///
    /// Mouse tracking is mutually exclusive in both tmux and the emulator, so the
    /// three tracking modes are applied in ascending order: a server that reports
    /// several (it should not) leaves the most permissive one on, which is what an
    /// application that enabled them in that order would have ended up with.
    fn apply_input_modes(&mut self, modes: &InputModes) {
        let switches: [(&str, bool); 8] = [
            ("\x1b[?1000h", modes.mouse_click),
            ("\x1b[?1002h", modes.mouse_drag),
            ("\x1b[?1003h", modes.mouse_motion),
            ("\x1b[?1006h", modes.mouse_sgr),
            ("\x1b[?1h", modes.application_cursor),
            ("\x1b=", modes.application_keypad),
            ("\x1b[4h", modes.insert),
            ("\x1b[?6h", modes.origin),
        ];
        for (sequence, enabled) in switches {
            if enabled {
                self.feed_control(sequence);
            }
        }
        // These two default to *on*, so they are the only resets a restore has to
        // emit.
        if !modes.show_cursor {
            self.feed_control("\x1b[?25l");
        }
        if !modes.line_wrap {
            self.feed_control("\x1b[?7l");
        }
        // `None` (a tmux older than 3.7) is not "off": leave the default alone.
        if modes.bracketed_paste == Some(true) {
            self.feed_control("\x1b[?2004h");
        }
    }

    /// Current cursor, taken from the emulator rather than from our own bookkeeping.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        self.screen().cursor()
    }

    /// Modes, taken from the emulator.
    #[must_use]
    pub fn modes(&self) -> Modes {
        self.screen().modes()
    }
}

impl TerminalCore for AlacrittyCore {
    fn feed(&mut self, bytes: &[u8]) {
        self.feed_bytes(bytes);
    }

    fn resize(&mut self, columns: u16, rows: u16) {
        let columns = columns.max(1);
        let rows = rows.max(1);
        self.term.resize(Size {
            columns: usize::from(columns),
            screen_lines: usize::from(rows),
        });
        self.columns = columns;
        self.rows = rows;
        // A resize reparses the grid layout; even when the emulator treats it as
        // a no-op the next report has to repaint over the old geometry.
        self.damage_pending = true;
        self.initial_full = false;
        self.fed_since_start = false;
    }

    fn take_damage(&mut self) -> Damage {
        let damage = match self.term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(lines) => Damage::Partial(
                lines
                    .map(|bounds| DamageLine {
                        row: bounds.line,
                        left: bounds.left,
                        right: bounds.right,
                    })
                    .collect(),
            ),
        };

        // The iterator borrows the terminal, so it has to be fully consumed (into
        // the owned report above) before the damage state can be reset.
        self.term.reset_damage();

        // A brand-new core is fully damaged by construction.  If live input was
        // fed before anyone asked for a report, express that state as one span
        // per row: the caller still learns the full screen is new, and the rows
        // the input touched are included.  Once the first report is taken, the
        // normal partial reports apply.
        let initial_full = std::mem::replace(&mut self.initial_full, false);
        let fed_since_start = std::mem::replace(&mut self.fed_since_start, false);
        let damage = if initial_full && fed_since_start && damage.is_full() {
            let right = usize::from(self.columns).saturating_sub(1);
            Damage::Partial(
                (0..usize::from(self.rows))
                    .map(|row| DamageLine {
                        row,
                        left: 0,
                        right,
                    })
                    .collect(),
            )
        } else {
            damage
        };

        // `Term::damage` always includes the cursor cell.  If nothing that could
        // move the cursor or repaint the grid happened since the last report,
        // that lone cell is all the emulator can report, and the honest answer is
        // "no changes".
        if self.damage_pending {
            self.damage_pending = false;
            damage
        } else {
            match damage {
                Damage::Full => Damage::Full,
                Damage::Partial(_) => Damage::Partial(Vec::new()),
            }
        }
    }

    fn restore(&mut self, snapshot: &ScreenSnapshot) -> Result<RestoreReport, RestoreError> {
        if !snapshot.is_usable() {
            return Err(RestoreError::EmptyGeometry);
        }

        // A restore must be silent: every response the emulator would like to send
        // while we repaint the screen is an artefact of the repaint, not an answer
        // the application is waiting for.  The guard holds a clone of the shared
        // listener, so suppression cannot borrow the terminal we are painting into.
        let listener = self.listener.clone();
        let guard = listener.suppress();

        // Start from a known state.  Without this, a second restore paints on top
        // of the previous snapshot: the non-alternate path replays at whatever
        // cursor was left behind, and the alternate path inherits modes (origin,
        // application cursor, ...) from the screen being replaced.  RIS clears the
        // grid, scrollback, scroll region, modes and cursor, and marks the whole
        // screen damaged.  It goes through the regular processor while responses
        // are suppressed, so anything it provokes is dropped and counted.
        self.feed_bytes(b"\x1bc");

        self.resize(snapshot.columns, snapshot.rows);

        // Alternate screen first: entering it clears it, so it has to happen before
        // anything is painted.
        let want_alternate = snapshot.alternate_on;
        let have_alternate = self.term.mode().contains(TermMode::ALT_SCREEN);
        let mut report = RestoreReport {
            alternate_screen: want_alternate,
            ..RestoreReport::default()
        };
        if want_alternate != have_alternate {
            self.feed_control(if want_alternate {
                "\x1b[?1049h"
            } else {
                "\x1b[?1049l"
            });
            report.alternate_switched = true;
        }

        // Reset the scroll region only.  Erase-display (`CSI 2 J`) is deliberately
        // *not* used: this core pushes a line into scrollback when it executes one,
        // which would add phantom history to every restored pane.
        self.feed_control("\x1b[r");

        let visible = snapshot.visible_lines();
        if want_alternate {
            // The alternate screen has no scrollback: paint the rows directly.
            for row in 0..usize::from(snapshot.rows) {
                self.feed_control(&format!("\x1b[{};1H", row + 1));
                if let Some(line) = visible.get(row) {
                    self.feed_bytes(line);
                    report.lines_painted += 1;
                }
                self.feed_control("\x1b[K");
            }
        } else {
            // Scrollback and screen are replayed as *one* stream, which is how the
            // pane produced them in the first place: feeding H history lines plus R
            // screen lines through an R-row grid leaves exactly H lines in history
            // and the captured screen in the viewport.  No line is fed after the
            // last one, so the viewport is not scrolled past the capture.
            for (index, line) in snapshot.lines.iter().enumerate() {
                if index > 0 {
                    self.feed_control("\r\n");
                }
                self.feed_bytes(line);
            }
            report.history_lines = snapshot.history_lines();
            report.lines_painted = visible.len();

            // A capture shorter than the pane (an empty pane, a race during resize)
            // leaves rows below it that must still be cleared.
            for row in snapshot.lines.len()..usize::from(snapshot.rows) {
                self.feed_control(&format!("\x1b[{};1H\x1b[K", row + 1));
            }
        }

        // Application input modes, after the paint and before the cursor:
        //
        // * after the paint, because insert mode would shift every line fed
        //   through it and origin mode would move the cursor to the home position
        //   mid-repaint;
        // * before the cursor, because setting origin mode homes the cursor, and
        //   the pane's reported cursor position is the one that must win.
        //
        // RIS above reset every mode to the emulator default, so only the *on*
        // switches have to be emitted: a mode tmux reports as off is already off.
        if let Some(modes) = &snapshot.input_modes {
            self.apply_input_modes(modes);
            report.input_modes_applied = true;
        }

        // Cursor last: painting moved it.
        let (column, row) = snapshot.cursor;
        self.set_cursor(column, row);
        report.cursor_set = true;

        drop(guard);
        report.responses_suppressed =
            usize::try_from(self.listener.effects().responses_suppressed).unwrap_or(usize::MAX);
        Ok(report)
    }

    fn screen(&self) -> ScreenView<'_> {
        ScreenView::new(&self.term)
    }

    fn cursor(&self) -> Cursor {
        self.screen().cursor()
    }

    fn modes(&self) -> Modes {
        self.screen().modes()
    }

    fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    fn set_history_limit(&mut self, lines: usize) {
        self.config.scrolling_history = lines;
        self.term.set_options(self.config.clone());
    }

    fn take_responses(&mut self) -> Vec<Vec<u8>> {
        self.listener.take_responses()
    }

    fn side_effects(&self) -> SideEffectCounts {
        self.listener.effects()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Scroll;

    #[test]
    fn cursor_row_is_viewport_relative_when_scrolled_back() {
        let mut core = AlacrittyCore::new(10, 3, 100);
        for index in 0..10 {
            core.feed(format!("line {index}\r\n").as_bytes());
        }

        let bottom = core.cursor();
        assert!(bottom.visible);
        assert_eq!(bottom.row, 2, "the cursor sits on the bottom viewport row");

        core.term.scroll_display(Scroll::Delta(1));
        let one_back = core.cursor();
        assert!(one_back.visible);
        assert_eq!(one_back.row, 1, "scrolling back moves the cursor row up");

        core.term.scroll_display(Scroll::Delta(2));
        let out_of_view = core.cursor();
        assert!(
            !out_of_view.visible,
            "a cursor above the viewport is hidden"
        );
        assert_eq!(out_of_view.row, 0, "the row is clamped to the nearest edge");
    }

    /// A snapshot of an 80x24 pane whose program asked for everything the server
    /// can report except the two modes no tmux release exposes.
    fn modes_snapshot() -> ScreenSnapshot {
        let mut snapshot = ScreenSnapshot::new(80, 24, vec![b"screen".to_vec()]);
        snapshot.cursor = (7, 3);
        snapshot.input_modes = Some(InputModes {
            mouse_click: false,
            mouse_drag: true,
            mouse_motion: false,
            mouse_sgr: true,
            application_cursor: true,
            application_keypad: true,
            show_cursor: false,
            insert: true,
            origin: true,
            line_wrap: false,
            bracketed_paste: Some(true),
        });
        snapshot
    }

    #[test]
    fn restore_applies_the_reported_input_modes() {
        let snapshot = modes_snapshot();
        let mut core = AlacrittyCore::new(80, 24, 100);
        let report = core.restore(&snapshot).expect("restore succeeds");

        assert!(report.input_modes_applied);
        let modes = core.modes();
        assert!(modes.mouse_drag, "DECSET 1002");
        assert!(!modes.mouse_click, "1000 was reported off");
        assert!(!modes.mouse_motion, "1003 was reported off");
        assert!(modes.mouse_sgr, "DECSET 1006");
        assert!(modes.mouse_reporting);
        assert!(modes.application_cursor, "DECCKM");
        assert!(modes.application_keypad, "DECKPAM");
        assert!(modes.insert, "IRM is a non-private mode");
        assert!(modes.origin, "DECSET 6");
        assert!(!modes.line_wrap, "DECSET 7 off");
        assert!(!modes.show_cursor, "DECSET 25 off");
        assert!(!core.cursor().visible, "the cursor view follows the mode");
        assert!(modes.bracketed_paste, "DECSET 2004");
        // The two modes no tmux release reports must stay unknown, i.e. off.
        assert!(!modes.focus_events);
        assert!(!modes.mouse_utf8);
        // Switching modes makes the emulator ask for a redraw, not for anything the
        // client would have to refuse.
        let effects = core.side_effects();
        assert_eq!(effects.other, 0, "unexpected side effects: {effects:?}");
    }

    #[test]
    fn the_reported_cursor_wins_over_origin_mode_homing() {
        // Setting origin mode homes the cursor, so a restore that applied the modes
        // after positioning it would put every restored pane's cursor at 0,0.
        let snapshot = modes_snapshot();
        let mut core = AlacrittyCore::new(80, 24, 100);
        core.restore(&snapshot).expect("restore succeeds");
        let cursor = core.cursor();
        assert_eq!((cursor.column, cursor.row), (7, 3));
    }

    #[test]
    fn restore_without_metadata_keeps_the_emulator_defaults() {
        let mut snapshot = ScreenSnapshot::new(10, 2, vec![b"x".to_vec()]);
        snapshot.cursor = (1, 1);
        let mut core = AlacrittyCore::new(10, 2, 100);
        // Leave a different mode set behind, so "did nothing" is visible.
        core.feed(b"\x1b[?1002h\x1b[?6h\x1b=\x1b[?25l");
        let report = core.restore(&snapshot).expect("restore succeeds");
        assert!(!report.input_modes_applied);
        let modes = core.modes();
        assert!(!modes.mouse_reporting);
        assert!(!modes.application_cursor);
        assert!(!modes.application_keypad);
        assert!(!modes.origin);
        assert!(modes.show_cursor);
        assert!(modes.line_wrap);
        assert!(!modes.bracketed_paste);
    }

    #[test]
    fn an_unreported_bracketed_paste_is_not_guessed() {
        // Before tmux 3.7 the flag expands to nothing. Turning it on "just in case"
        // would make the client send `CSI 200 ~` to an application that never asked
        // for it, which is worse than not using bracketed paste.
        let mut snapshot = ScreenSnapshot::new(10, 2, vec![b"x".to_vec()]);
        snapshot.input_modes = Some(InputModes {
            bracketed_paste: None,
            ..InputModes::default()
        });
        let mut core = AlacrittyCore::new(10, 2, 100);
        core.restore(&snapshot).expect("restore succeeds");
        assert!(!core.modes().bracketed_paste);
    }

    #[test]
    fn restore_is_idempotent_for_input_modes() {
        let snapshot = modes_snapshot();
        let mut core = AlacrittyCore::new(80, 24, 100);
        core.restore(&snapshot).expect("restore succeeds");
        let expected = core.modes();

        // A second core left in the opposite state, then restored from the same
        // snapshot: RIS must clear what the previous state had on, and the snapshot
        // must turn back on what it reports.
        let mut reused = AlacrittyCore::new(80, 24, 100);
        reused.feed(b"\x1b[?1000h\x1b[?1003h\x1b[?1006h\x1b[?7h\x1b[?25h\x1b[>4h");
        assert!(reused.modes().mouse_motion);
        reused.restore(&snapshot).expect("restore succeeds");
        assert_eq!(reused.modes(), expected);
        assert_eq!(reused.cursor(), core.cursor());
    }
}
