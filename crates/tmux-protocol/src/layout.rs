//! tmux layout strings.
//!
//! A layout is a tree, not a binary split chain: tmux prints one node per region
//! and a region may hold any number of children.  Modelling it as a tree from the
//! start avoids the classic bug where a third pane in a split cannot be
//! represented and the GUI silently rearranges the user's window.
//!
//! Grammar (derived from the format printed by `#{window_layout}` and by
//! `%layout-change`, and pinned by the recorded fixtures):
//!
//! ```text
//! layout  := checksum ',' node
//! node    := W 'x' H ',' X ',' Y ( ',' pane | '[' node (',' node)* ']'
//!                                          | '{' node (',' node)* '}' )
//! checksum:= 4 hex digits
//! ```
//!
//! The bracket mapping is **not** what one would guess: `{` introduces a left/right
//! arrangement (children side by side, `split-window -h`) and `[` a top/bottom one
//! (`split-window -v`).  Recorded from tmux 3.4:
//!
//! ```text
//! split-window -h -> ...{40x24,0,0,1,39x24,41,0,2}     (x differs: side by side)
//! split-window -v -> ...[39x12,41,0,2,39x11,41,13,3]   (y differs: stacked)
//! ```
//!
//! `tests/fixtures.rs` re-asserts this against the captures, so a release that
//! changes it fails the build before the GUI can mis-render.

use crate::ids::PaneId;

/// How the children of a split are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Children are side by side (`split-window -h`).
    LeftRight,
    /// Children are stacked (`split-window -v`).
    TopBottom,
}

/// Geometry shared by every node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// Width in terminal cells.
    pub width: u32,
    /// Height in terminal cells.
    pub height: u32,
    /// Column of the top-left corner.
    pub x: u32,
    /// Row of the top-left corner.
    pub y: u32,
}

/// A parsed layout tree node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    /// A region occupied by exactly one pane.
    Pane {
        /// Geometry of the pane.
        cell: Cell,
        /// Pane identifier (the number after `%`).
        pane: PaneId,
    },
    /// A region split into two or more children.
    Split {
        /// How the children are arranged.
        orientation: Orientation,
        /// Geometry of the whole region.
        cell: Cell,
        /// Child regions, in tmux's order.
        children: Vec<LayoutNode>,
    },
}

impl LayoutNode {
    /// Geometry of this node.
    #[must_use]
    pub const fn cell(&self) -> Cell {
        match self {
            Self::Pane { cell, .. } | Self::Split { cell, .. } => *cell,
        }
    }

    /// Depth of the subtree (a leaf has depth 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { children, .. } => 1 + children.iter().map(Self::depth).max().unwrap_or(0),
        }
    }

    /// Appends every pane in this subtree, in layout order.
    pub fn collect_panes(&self, out: &mut Vec<(PaneId, Cell)>) {
        match self {
            Self::Pane { cell, pane } => out.push((*pane, *cell)),
            Self::Split { children, .. } => {
                for child in children {
                    child.collect_panes(out);
                }
            }
        }
    }

    /// Number of panes in this subtree.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        match self {
            Self::Pane { .. } => 1,
            Self::Split { children, .. } => children.iter().map(Self::pane_count).sum(),
        }
    }
}

/// A complete layout string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// Checksum tmux printed alongside the layout (not verified here: it is the
    /// server's own integrity value, and the GUI never edits layouts locally).
    pub checksum: u32,
    /// Root node, carrying the window geometry.
    pub root: LayoutNode,
}

impl Layout {
    /// Parses a layout string such as
    /// `bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]`.
    ///
    /// # Errors
    ///
    /// Returns [`LayoutError`] when the string is truncated, contains a
    /// non-numeric field, mixes up the separators, or describes a split with
    /// fewer than two children.
    pub fn parse(input: &[u8]) -> Result<Self, LayoutError> {
        let input = trim_ascii(input);
        let mut cursor = Cursor { input, pos: 0 };
        let checksum = cursor.parse_hex4()?;
        cursor.expect(b',')?;
        let root = cursor.parse_node()?;
        if cursor.pos != cursor.input.len() {
            return Err(LayoutError::TrailingBytes { offset: cursor.pos });
        }
        Ok(Self { checksum, root })
    }

    /// Every pane in the layout, with its geometry.
    #[must_use]
    pub fn panes(&self) -> Vec<(PaneId, Cell)> {
        let mut out = Vec::new();
        self.root.collect_panes(&mut out);
        out
    }

    /// Window geometry as reported by the root node.
    #[must_use]
    pub const fn cell(&self) -> Cell {
        self.root.cell()
    }

    /// Looks up the geometry of one pane.
    #[must_use]
    pub fn pane_cell(&self, pane: PaneId) -> Option<Cell> {
        self.panes()
            .into_iter()
            .find_map(|(id, cell)| (id == pane).then_some(cell))
    }
}

/// Why a layout string could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutError {
    /// The string ended in the middle of a node.
    UnexpectedEnd,
    /// A byte was expected at a specific place.
    Expected {
        /// What the parser expected.
        what: &'static str,
        /// Offset where it looked.
        offset: usize,
    },
    /// A numeric field was malformed or too large.
    BadNumber {
        /// Offset of the field.
        offset: usize,
    },
    /// A split node had fewer than two children.
    DegenerateSplit {
        /// Offset of the opening bracket.
        offset: usize,
    },
    /// Bytes were left over after a complete layout.
    TrailingBytes {
        /// Offset of the first leftover byte.
        offset: usize,
    },
    /// A node nested deeper than [`Layout::parse`] accepts.
    TooDeep,
}

impl core::fmt::Display for LayoutError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnexpectedEnd => write!(f, "unexpected end of layout"),
            Self::Expected { what, offset } => {
                write!(f, "expected {what} at offset {offset}")
            }
            Self::BadNumber { offset } => write!(f, "bad number at offset {offset}"),
            Self::DegenerateSplit { offset } => {
                write!(f, "split at offset {offset} has fewer than two children")
            }
            Self::TrailingBytes { offset } => write!(f, "trailing bytes at offset {offset}"),
            Self::TooDeep => write!(f, "layout nested too deeply"),
        }
    }
}

impl std::error::Error for LayoutError {}

/// Nesting bound: tmux would need a pathological window to exceed this.
const MAX_DEPTH: usize = 64;

struct Cursor<'a> {
    input: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), LayoutError> {
        match self.peek() {
            Some(found) if found == byte => {
                self.pos += 1;
                Ok(())
            }
            Some(_) => Err(LayoutError::Expected {
                what: match byte {
                    b',' => "','",
                    b'x' => "'x'",
                    b']' => "']'",
                    b'}' => "'}'",
                    _ => "separator",
                },
                offset: self.pos,
            }),
            None => Err(LayoutError::UnexpectedEnd),
        }
    }

    fn parse_number(&mut self, max_digits: usize) -> Result<u32, LayoutError> {
        let start = self.pos;
        let mut value: u32 = 0;
        let mut digits = 0;
        while let Some(b) = self.peek() {
            if !b.is_ascii_digit() {
                break;
            }
            if digits == max_digits {
                return Err(LayoutError::BadNumber { offset: start });
            }
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u32::from(b - b'0')))
                .ok_or(LayoutError::BadNumber { offset: start })?;
            digits += 1;
            self.pos += 1;
        }
        if digits == 0 {
            return Err(LayoutError::BadNumber { offset: start });
        }
        Ok(value)
    }

    fn parse_hex4(&mut self) -> Result<u32, LayoutError> {
        let start = self.pos;
        let mut value: u32 = 0;
        for _ in 0..4 {
            let Some(b) = self.peek() else {
                return Err(LayoutError::UnexpectedEnd);
            };
            let digit = match b {
                b'0'..=b'9' => u32::from(b - b'0'),
                b'a'..=b'f' => u32::from(b - b'a') + 10,
                b'A'..=b'F' => u32::from(b - b'A') + 10,
                _ => return Err(LayoutError::BadNumber { offset: start }),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn parse_node(&mut self) -> Result<LayoutNode, LayoutError> {
        self.parse_node_at(0)
    }

    fn parse_node_at(&mut self, depth: usize) -> Result<LayoutNode, LayoutError> {
        if depth > MAX_DEPTH {
            return Err(LayoutError::TooDeep);
        }
        let width = self.parse_number(6)?;
        self.expect(b'x')?;
        let height = self.parse_number(6)?;
        self.expect(b',')?;
        let x = self.parse_number(6)?;
        self.expect(b',')?;
        let y = self.parse_number(6)?;
        let cell = Cell {
            width,
            height,
            x,
            y,
        };
        match self.peek() {
            Some(b'[') | Some(b'{') => {
                let orientation = if self.peek() == Some(b'[') {
                    Orientation::TopBottom
                } else {
                    Orientation::LeftRight
                };
                let open = self.pos;
                self.pos += 1;
                let close = if orientation == Orientation::TopBottom {
                    b']'
                } else {
                    b'}'
                };
                let mut children = Vec::new();
                loop {
                    if self.peek() == Some(close) {
                        self.pos += 1;
                        break;
                    }
                    children.push(self.parse_node_at(depth + 1)?);
                    match self.peek() {
                        Some(b',') => self.pos += 1,
                        Some(b) if b == close => {
                            self.pos += 1;
                            break;
                        }
                        Some(_) => {
                            return Err(LayoutError::Expected {
                                what: "',' or closing bracket",
                                offset: self.pos,
                            })
                        }
                        None => return Err(LayoutError::UnexpectedEnd),
                    }
                }
                if children.len() < 2 {
                    return Err(LayoutError::DegenerateSplit { offset: open });
                }
                Ok(LayoutNode::Split {
                    orientation,
                    cell,
                    children,
                })
            }
            Some(_) => {
                self.expect(b',')?;
                let pane = self.parse_number(10)?;
                Ok(LayoutNode::Pane {
                    cell,
                    pane: PaneId::new(pane),
                })
            }
            None => Err(LayoutError::UnexpectedEnd),
        }
    }
}

fn trim_ascii(input: &[u8]) -> &[u8] {
    let start = input
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(input.len());
    let end = input
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &input[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_single_pane() {
        let layout = Layout::parse(b"bb62,80x24,0,0,0").unwrap();
        assert_eq!(layout.checksum, 0xbb62);
        assert_eq!(
            layout.root,
            LayoutNode::Pane {
                cell: Cell {
                    width: 80,
                    height: 24,
                    x: 0,
                    y: 0
                },
                pane: PaneId::new(0)
            }
        );
        assert_eq!(layout.panes().len(), 1);
    }

    #[test]
    fn parses_a_stacked_split() {
        let layout = Layout::parse(b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]").unwrap();
        let LayoutNode::Split {
            orientation,
            children,
            ..
        } = &layout.root
        else {
            panic!("expected a split");
        };
        assert_eq!(
            *orientation,
            Orientation::TopBottom,
            "'[' stacks children (recorded from tmux 3.4)"
        );
        assert_eq!(children.len(), 2);
        assert_eq!(
            layout.panes(),
            vec![
                (
                    PaneId::new(0),
                    Cell {
                        width: 80,
                        height: 12,
                        x: 0,
                        y: 0
                    }
                ),
                (
                    PaneId::new(1),
                    Cell {
                        width: 80,
                        height: 11,
                        x: 0,
                        y: 13
                    }
                ),
            ]
        );
    }

    #[test]
    fn parses_a_side_by_side_split() {
        let layout = Layout::parse(b"1111,80x24,0,0{39x24,0,0,0,40x24,40,0,1}").unwrap();
        let LayoutNode::Split { orientation, .. } = layout.root else {
            panic!("expected a split");
        };
        assert_eq!(
            orientation,
            Orientation::LeftRight,
            "'{{' puts children side by side (recorded from tmux 3.4)"
        );
    }

    #[test]
    fn parses_nested_splits() {
        let layout = Layout::parse(
            b"9c1a,203x51,0,0{101x51,0,0,0,101x51,102,0[101x25,102,0,1,101x25,102,26,2]}",
        )
        .unwrap();
        assert_eq!(layout.root.pane_count(), 3);
        assert_eq!(layout.root.depth(), 3);
        assert_eq!(layout.pane_cell(PaneId::new(2)).unwrap().y, 26);
    }

    #[test]
    fn tolerates_surrounding_whitespace() {
        assert!(Layout::parse(b" bb62,80x24,0,0,0 \n").is_ok());
    }

    #[test]
    fn rejects_malformed_layouts() {
        assert_eq!(Layout::parse(b""), Err(LayoutError::UnexpectedEnd));
        assert_eq!(
            Layout::parse(b"bb62,80x24,0,0"),
            Err(LayoutError::UnexpectedEnd)
        );
        assert!(matches!(
            Layout::parse(b"zzzz,80x24,0,0,0"),
            Err(LayoutError::BadNumber { .. })
        ));
        assert!(matches!(
            Layout::parse(b"bb62,80x24,0,0[80x12,0,0,0]"),
            Err(LayoutError::DegenerateSplit { .. })
        ));
        assert!(matches!(
            Layout::parse(b"bb62,80x24,0,0,0extra"),
            Err(LayoutError::TrailingBytes { .. })
        ));
        assert!(matches!(
            Layout::parse(b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1}"),
            Err(LayoutError::Expected { .. } | LayoutError::BadNumber { .. })
        ));
    }

    #[test]
    fn rejects_pathological_nesting_without_recursing_forever() {
        let mut input = b"bb62,80x24,0,0".to_vec();
        for _ in 0..200 {
            input.extend_from_slice(b"[80x24,0,0");
        }
        assert_eq!(Layout::parse(&input), Err(LayoutError::TooDeep));
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert!(matches!(
            Layout::parse(b"bb62,99999999999x24,0,0,0"),
            Err(LayoutError::BadNumber { .. })
        ));
    }
}
