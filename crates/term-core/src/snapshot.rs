//! What a tmux pane tells us, and what a restore has to put back.
//!
//! A `capture-pane` gives text and styles; it does **not** give the cursor, the
//! alternate-screen flag or the history size.  Those come from the per-pane
//! metadata queries in `tmux_client::sync`, and this module is where the two are
//! joined.

/// Everything needed to make an emulator show what a pane is showing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScreenSnapshot {
    /// Pane width in cells.
    pub columns: u16,
    /// Pane height in cells.
    pub rows: u16,
    /// Captured lines, escapes preserved, history first: the last `rows` lines are
    /// the visible screen.
    pub lines: Vec<Vec<u8>>,
    /// Cursor column and row, zero-based, as reported by `#{cursor_x}`/`#{cursor_y}`.
    pub cursor: (u16, u16),
    /// True when the pane is on the alternate screen.
    pub alternate_on: bool,
    /// Lines the server still holds for this pane.
    pub history_size: u32,
    /// The pane's configured history limit.
    pub history_limit: u32,
    /// True while the pane is inside a tmux mode (copy mode, ...).
    pub in_mode: bool,
    /// Name of that mode when the server reports one.
    pub mode_name: Option<Vec<u8>>,
    /// True when the pane's process has exited.
    pub dead: bool,
}

impl ScreenSnapshot {
    /// Builds a snapshot from a capture and the pane geometry.
    #[must_use]
    pub fn new(columns: u16, rows: u16, lines: Vec<Vec<u8>>) -> Self {
        Self {
            columns,
            rows,
            lines,
            ..Self::default()
        }
    }

    /// Number of captured lines that belong to the scrollback.
    ///
    /// A capture taken with `-S -` contains the history followed by the visible
    /// screen, so the split point is `total - rows`.
    #[must_use]
    pub fn history_lines(&self) -> usize {
        self.lines.len().saturating_sub(usize::from(self.rows))
    }

    /// The lines that are currently visible.
    #[must_use]
    pub fn visible_lines(&self) -> &[Vec<u8>] {
        let start = self.history_lines();
        &self.lines[start..]
    }

    /// The lines that belong to the scrollback.
    #[must_use]
    pub fn scrollback_lines(&self) -> &[Vec<u8>] {
        let end = self.history_lines();
        &self.lines[..end]
    }

    /// True when the snapshot can be restored at all.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.columns > 0 && self.rows > 0
    }
}

/// Why a restore could not be performed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// The snapshot has no geometry, so there is nothing to restore into.
    EmptyGeometry,
    /// The core refused the resize.
    ResizeRejected {
        /// Requested columns.
        columns: u16,
        /// Requested rows.
        rows: u16,
    },
}

impl core::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyGeometry => write!(f, "snapshot has no geometry"),
            Self::ResizeRejected { columns, rows } => {
                write!(f, "core rejected a resize to {columns}x{rows}")
            }
        }
    }
}

impl std::error::Error for RestoreError {}

/// What a restore actually did.
///
/// Reported rather than implied: a caller must be able to tell "screen
/// resynchronised" apart from "history continuity guaranteed".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreReport {
    /// Visible lines painted.
    pub lines_painted: usize,
    /// Scrollback lines pushed back into history.
    pub history_lines: usize,
    /// Whether the cursor was repositioned explicitly.
    pub cursor_set: bool,
    /// Whether the alternate screen was switched.
    pub alternate_switched: bool,
    /// Responses the emulator generated during the repaint and that were dropped.
    pub responses_suppressed: usize,
    /// True when the pane is on the alternate screen, so the scrollback is not
    /// meaningful.
    pub alternate_screen: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(count: usize) -> Vec<Vec<u8>> {
        (0..count)
            .map(|i| format!("line {i}").into_bytes())
            .collect()
    }

    #[test]
    fn splits_a_capture_into_history_and_screen() {
        let snapshot = ScreenSnapshot::new(80, 3, lines(10));
        assert_eq!(snapshot.history_lines(), 7);
        assert_eq!(snapshot.scrollback_lines().len(), 7);
        assert_eq!(snapshot.visible_lines().len(), 3);
        assert_eq!(snapshot.visible_lines()[0], b"line 7");
    }

    #[test]
    fn a_capture_without_history_is_all_screen() {
        let snapshot = ScreenSnapshot::new(80, 24, lines(5));
        assert_eq!(snapshot.history_lines(), 0);
        assert_eq!(snapshot.visible_lines().len(), 5);
    }

    #[test]
    fn geometry_is_validated() {
        assert!(!ScreenSnapshot::new(0, 24, lines(1)).is_usable());
        assert!(!ScreenSnapshot::new(80, 0, lines(1)).is_usable());
        assert!(ScreenSnapshot::new(80, 24, lines(1)).is_usable());
    }
}
