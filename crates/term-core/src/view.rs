//! Read-only export of the screen.
//!
//! A renderer needs cells, attributes, the cursor and the modes — and it must be
//! able to read them **without** taking a mutable borrow of the terminal, so that
//! drawing can never block input processing (gate 4 in `docs/adr/0002`).
//!
//! The types here are the project's own.  Nothing above this crate names an
//! emulator type, so the choice made in `docs/adr/0004` stays reversible.

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{
    Color as AnsiColor, CursorShape as AnsiCursorShape, NamedColor,
};

use crate::alacritty_core::ListenerHandle;

/// A colour a renderer can resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The theme's default foreground or background.
    Default,
    /// A palette index (0-15 are the ANSI colours, 16-255 the extended palette).
    Indexed(u8),
    /// A direct colour.
    Rgb(u8, u8, u8),
}

impl Color {
    fn from_ansi(color: AnsiColor) -> Self {
        match color {
            AnsiColor::Spec(rgb) => Self::Rgb(rgb.r, rgb.g, rgb.b),
            AnsiColor::Indexed(index) => Self::Indexed(index),
            AnsiColor::Named(named) => Self::from_named(named),
        }
    }

    fn from_named(named: NamedColor) -> Self {
        match named {
            NamedColor::Black => Self::Indexed(0),
            NamedColor::Red => Self::Indexed(1),
            NamedColor::Green => Self::Indexed(2),
            NamedColor::Yellow => Self::Indexed(3),
            NamedColor::Blue => Self::Indexed(4),
            NamedColor::Magenta => Self::Indexed(5),
            NamedColor::Cyan => Self::Indexed(6),
            NamedColor::White => Self::Indexed(7),
            NamedColor::BrightBlack => Self::Indexed(8),
            NamedColor::BrightRed => Self::Indexed(9),
            NamedColor::BrightGreen => Self::Indexed(10),
            NamedColor::BrightYellow => Self::Indexed(11),
            NamedColor::BrightBlue => Self::Indexed(12),
            NamedColor::BrightMagenta => Self::Indexed(13),
            NamedColor::BrightCyan => Self::Indexed(14),
            NamedColor::BrightWhite => Self::Indexed(15),
            // Dim variants and the special slots (foreground, background, cursor,
            // text) resolve through the theme.
            _ => Self::Default,
        }
    }
}

/// Text attributes of one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellFlags {
    /// Bold.
    pub bold: bool,
    /// Italic.
    pub italic: bool,
    /// Underlined.
    pub underline: bool,
    /// Reverse video.
    pub inverse: bool,
    /// Faint.
    pub dim: bool,
    /// Concealed.
    pub hidden: bool,
    /// Struck through.
    pub strikeout: bool,
    /// The cell holds a double-width character.
    pub wide: bool,
    /// The cell is the right-hand half of a double-width character.
    pub wide_spacer: bool,
}

impl CellFlags {
    fn from_flags(flags: Flags) -> Self {
        Self {
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline: flags.contains(Flags::UNDERLINE),
            inverse: flags.contains(Flags::INVERSE),
            dim: flags.contains(Flags::DIM),
            hidden: flags.contains(Flags::HIDDEN),
            strikeout: flags.contains(Flags::STRIKEOUT),
            wide: flags.contains(Flags::WIDE_CHAR),
            wide_spacer: flags.contains(Flags::WIDE_CHAR_SPACER),
        }
    }
}

/// One cell, as a renderer sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellView {
    /// The character in the cell.
    pub ch: char,
    /// Combining characters that belong to it.
    pub zerowidth: Vec<char>,
    /// Foreground colour.
    pub fg: Color,
    /// Background colour.
    pub bg: Color,
    /// Attributes.
    pub flags: CellFlags,
}

impl CellView {
    fn new(cell: &Cell) -> Self {
        Self {
            ch: cell.c,
            zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
            fg: Color::from_ansi(cell.fg),
            bg: Color::from_ansi(cell.bg),
            flags: CellFlags::from_flags(cell.flags),
        }
    }

    /// True when the cell carries nothing a renderer would draw.
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.ch == ' ' && self.zerowidth.is_empty()
    }
}

/// Cursor shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// Filled block.
    #[default]
    Block,
    /// Underline.
    Underline,
    /// Vertical bar.
    Beam,
    /// Hollow block.
    HollowBlock,
    /// Hidden.
    Hidden,
}

/// Cursor position and shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// Row inside the viewport, zero-based.
    pub row: usize,
    /// Column, zero-based.
    pub column: usize,
    /// Shape to draw.
    pub shape: CursorShape,
    /// Whether the application asked for the cursor to be visible.
    pub visible: bool,
}

/// Modes that change how input must be encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modes {
    /// The pane is on the alternate screen.
    pub alternate_screen: bool,
    /// Bracketed paste is enabled, so pasted text must be wrapped.
    pub bracketed_paste: bool,
    /// Cursor keys send application sequences.
    pub application_cursor: bool,
    /// The keypad sends application sequences.
    pub application_keypad: bool,
    /// Some mouse reporting mode is active.
    pub mouse_reporting: bool,
    /// Mouse clicks are reported (DECSET 1000).
    pub mouse_click: bool,
    /// Button drags are reported (DECSET 1002).
    pub mouse_drag: bool,
    /// All motions are reported (DECSET 1003).
    pub mouse_motion: bool,
    /// Mouse reports use the SGR encoding (DECSET 1006).
    pub mouse_sgr: bool,
    /// Mouse reports use the UTF-8 coordinate encoding (DECSET 1005).
    pub mouse_utf8: bool,
    /// The cursor is visible.
    pub show_cursor: bool,
    /// Insert mode.
    pub insert: bool,
    /// Origin mode.
    pub origin: bool,
    /// Automatic line wrapping.
    pub line_wrap: bool,
    /// Focus in/out events are reported.
    pub focus_events: bool,
}

impl Modes {
    fn from_term_mode(mode: TermMode) -> Self {
        Self {
            alternate_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            application_cursor: mode.contains(TermMode::APP_CURSOR),
            application_keypad: mode.contains(TermMode::APP_KEYPAD),
            mouse_reporting: mode.intersects(
                TermMode::MOUSE_REPORT_CLICK | TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION,
            ),
            mouse_click: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: mode.contains(TermMode::MOUSE_DRAG),
            mouse_motion: mode.contains(TermMode::MOUSE_MOTION),
            mouse_sgr: mode.contains(TermMode::SGR_MOUSE),
            mouse_utf8: mode.contains(TermMode::UTF8_MOUSE),
            show_cursor: mode.contains(TermMode::SHOW_CURSOR),
            insert: mode.contains(TermMode::INSERT),
            origin: mode.contains(TermMode::ORIGIN),
            line_wrap: mode.contains(TermMode::LINE_WRAP),
            focus_events: mode.contains(TermMode::FOCUS_IN_OUT),
        }
    }
}

/// One row of the viewport.
pub struct LineView<'a> {
    row: &'a alacritty_terminal::grid::Row<Cell>,
    columns: usize,
}

impl<'a> LineView<'a> {
    /// The cells of this row, left to right.
    pub fn cells(&self) -> impl Iterator<Item = CellView> + 'a {
        let row = self.row;
        let columns = self.columns;
        (0..columns).map(move |column| CellView::new(&row[Column(column)]))
    }

    /// The row as text.
    ///
    /// Wide-character spacers are skipped and combining characters are appended to
    /// the character they belong to, which is what a text comparison against a
    /// `capture-pane` needs.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.columns);
        for column in 0..self.columns {
            let cell = &self.row[Column(column)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                for ch in zerowidth {
                    text.push(*ch);
                }
            }
        }
        text
    }
}

/// Read-only view of a terminal's current screen.
#[derive(Clone, Copy)]
pub struct ScreenView<'a> {
    term: &'a Term<ListenerHandle>,
}

impl<'a> ScreenView<'a> {
    pub(crate) fn new(term: &'a Term<ListenerHandle>) -> Self {
        Self { term }
    }

    /// Width in columns.
    #[must_use]
    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    /// Height in rows.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.term.grid().screen_lines()
    }

    /// Lines held in scrollback.
    #[must_use]
    pub fn history_size(&self) -> usize {
        self.term.grid().history_size()
    }

    /// How far the viewport is scrolled back.
    #[must_use]
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// The cursor.
    ///
    /// The row is relative to the viewport: when the screen is scrolled back, a
    /// cursor that belongs to the visible region moves down with the text.  When
    /// the cursor itself is scrolled out of view, `visible` is false and the row
    /// is clamped to the nearest edge.
    #[must_use]
    pub fn cursor(&self) -> Cursor {
        let content = self.term.renderable_content();
        let shape = match content.cursor.shape {
            AnsiCursorShape::Block => CursorShape::Block,
            AnsiCursorShape::Underline => CursorShape::Underline,
            AnsiCursorShape::Beam => CursorShape::Beam,
            AnsiCursorShape::HollowBlock => CursorShape::HollowBlock,
            AnsiCursorShape::Hidden => CursorShape::Hidden,
        };
        let rows = self.rows();
        let offset = i32::try_from(self.term.grid().display_offset()).unwrap_or(i32::MAX);
        let viewport_line = content.cursor.point.line.0.saturating_sub(offset);
        let (row, in_viewport) = if viewport_line < 0 {
            (0, false)
        } else {
            let line = usize::try_from(viewport_line).unwrap_or(usize::MAX);
            if line < rows {
                (line, true)
            } else {
                (rows.saturating_sub(1), false)
            }
        };
        Cursor {
            row,
            column: content.cursor.point.column.0,
            shape,
            visible: in_viewport
                && content.mode.contains(TermMode::SHOW_CURSOR)
                && shape != CursorShape::Hidden,
        }
    }

    /// The modes that affect input encoding.
    #[must_use]
    pub fn modes(&self) -> Modes {
        Modes::from_term_mode(*self.term.mode())
    }

    /// One visible row, or `None` when it is outside the viewport.
    #[must_use]
    pub fn line(&self, row: usize) -> Option<LineView<'a>> {
        if row >= self.rows() {
            return None;
        }
        let line = Line(i32::try_from(row).ok()?) - self.display_offset();
        Some(LineView {
            row: &self.term.grid()[line],
            columns: self.columns(),
        })
    }

    /// One cell, or `None` when it is outside the viewport.
    #[must_use]
    pub fn cell(&self, row: usize, column: usize) -> Option<CellView> {
        let line = self.line(row)?;
        if column >= line.columns {
            return None;
        }
        Some(CellView::new(&line.row[Column(column)]))
    }

    /// One row as text.
    #[must_use]
    pub fn line_text(&self, row: usize) -> String {
        self.line(row).map(|line| line.text()).unwrap_or_default()
    }

    /// The whole visible screen as text, one line per row, trailing blanks removed.
    #[must_use]
    pub fn dump_text(&self) -> String {
        let mut out = String::new();
        for row in 0..self.rows() {
            out.push_str(self.line_text(row).trim_end());
            if row + 1 < self.rows() {
                out.push('\n');
            }
        }
        out
    }
}
