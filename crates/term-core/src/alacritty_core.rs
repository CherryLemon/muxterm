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
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{Handler, Processor};

use crate::snapshot::{RestoreError, RestoreReport, ScreenSnapshot};
use crate::view::{Cursor, Modes, ScreenView};
use crate::TerminalCore;

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
    }

    /// Feeds an ASCII control sequence written by this adapter.
    fn feed_control(&mut self, sequence: &str) {
        self.feed_bytes(sequence.as_bytes());
    }

    /// Moves the cursor explicitly, outside the byte stream.
    fn set_cursor(&mut self, column: u16, row: u16) {
        self.term.goto(i32::from(row), usize::from(column));
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
                    let line = line.clone();
                    self.feed_bytes(&line);
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
                let line = line.clone();
                self.feed_bytes(&line);
            }
            report.history_lines = snapshot.history_lines();
            report.lines_painted = visible.len();

            // A capture shorter than the pane (an empty pane, a race during resize)
            // leaves rows below it that must still be cleared.
            for row in snapshot.lines.len()..usize::from(snapshot.rows) {
                self.feed_control(&format!("\x1b[{};1H\x1b[K", row + 1));
            }
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
