//! Snapshot and resynchronisation.
//!
//! # The watermark rule
//!
//! A reconnect must restore *state*, not just text.  The design here rests on one
//! property of control mode: everything the server sends to a control client goes
//! through one ordered stream.  A `capture-pane` response is therefore a
//! watermark — pane output written to the stream *before* the response is already
//! contained in the captured screen, and output written *after* it is not.
//!
//! So the rule is:
//!
//! * while a pane's snapshot has not been loaded, its `%output` events are
//!   **dropped** (they are already in the snapshot) and counted;
//! * once the snapshot for that pane has been loaded, every later `%output` is
//!   applied in stream order.
//!
//! This is the opposite of "replay everything received while connecting", which
//! double-applies output and corrupts full-screen programs.  `docs/spec/state-sync.md`
//! records how the PoC verifies the rule (`scenario reconnect`).
//!
//! # Fixed initialisation order
//!
//! The order below is part of the specification, not an implementation detail: it
//! is what makes each response an unambiguous watermark for exactly one pane.
//!
//! 1. `display-message -p <marker>` — separates the server's own initialisation
//!    responses from ours (see [`crate::session`]).
//! 2. `list-sessions`, 3. `list-windows -a`, 4. `list-panes -a`
//! 5. per pane: `capture-pane` — the watermark, 6. per pane: pane metadata.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use tmux_protocol::command::Command;
use tmux_protocol::ids::{PaneId, SessionId, WindowId};
use tmux_protocol::limits::Limits;
use tmux_protocol::queue::Response;

use crate::model::{ModelChange, PaneState, RemoteModel};

/// Which part of the snapshot a command belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    /// `display-message -p <marker>` — marks the end of unsolicited output.
    Barrier,
    /// `list-sessions`.
    ListSessions,
    /// `list-windows -a`.
    ListWindows,
    /// `list-panes -a`.
    ListPanes,
    /// `capture-pane` for one pane: the snapshot watermark.
    CapturePane,
    /// `display-message -p` cursor/mode/history metadata for one pane.
    PaneMeta,
}

/// A command belonging to the snapshot, with the context to interpret it.
#[derive(Debug, Clone)]
pub struct TaggedCommand {
    /// Command to schedule.
    pub command: Command,
    /// Snapshot stage.
    pub tag: Tag,
    /// Pane the command refers to, for pane-scoped stages.
    pub pane: Option<PaneId>,
}

/// Statistics of one completed snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SyncStats {
    /// Sessions discovered.
    pub sessions: usize,
    /// Windows discovered.
    pub windows: usize,
    /// Panes discovered.
    pub panes: usize,
    /// Panes whose screen was captured.
    pub captures: usize,
}

/// What the engine learned from one response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncOutcome {
    /// The snapshot advanced but produced nothing to hand to a terminal.
    Progress {
        /// Model changes caused by this response.
        changes: Vec<ModelChange>,
    },
    /// A pane's screen was captured: load it into that pane's terminal emulator
    /// **now**, before processing the next event, because the response is the
    /// watermark for everything that follows.
    PaneCaptured {
        /// Pane whose screen was captured.
        pane: PaneId,
        /// Captured lines, including any escape sequences tmux emitted (`-e`).
        lines: Vec<Vec<u8>>,
        /// Model changes caused by this response.
        changes: Vec<ModelChange>,
    },
    /// The snapshot is complete.
    Done {
        /// Totals.
        stats: SyncStats,
        /// Model changes caused by the final response.
        changes: Vec<ModelChange>,
    },
}

/// Why a snapshot failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// The server answered a snapshot command with `%error`.
    CommandFailed {
        /// Stage that failed.
        tag: Tag,
        /// First line of the error output.
        message: String,
    },
    /// A response could not be interpreted.
    Malformed {
        /// Stage whose response was malformed.
        tag: Tag,
        /// What was wrong.
        detail: String,
    },
    /// A response was cut short by [`tmux_protocol::Limits`].
    ///
    /// A truncated `capture-pane` is especially dangerous: it contains the
    /// *oldest* history lines, not the visible screen, so applying it would paint
    /// a screen that is minutes or hours stale.
    Truncated {
        /// Stage whose response was truncated.
        tag: Tag,
    },
    /// The connection ended before the snapshot completed.
    Interrupted,
}

impl core::fmt::Display for SyncError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CommandFailed { tag, message } => write!(f, "{tag:?} failed: {message}"),
            Self::Malformed { tag, detail } => write!(f, "{tag:?} response malformed: {detail}"),
            Self::Truncated { tag } => {
                write!(f, "{tag:?} response exceeded the configured limits")
            }
            Self::Interrupted => write!(f, "connection closed during snapshot"),
        }
    }
}

impl std::error::Error for SyncError {}

/// Drives the initial snapshot, one command at a time.
#[derive(Debug, Clone)]
pub struct SyncEngine {
    marker: String,
    /// Upper bound on the number of body lines one response may carry.
    max_capture_lines: u32,
    started: bool,
    finished: bool,
    panes: Vec<PaneId>,
    /// Height of each pane as reported by `list-panes`, used to bound its capture.
    pane_heights: BTreeMap<PaneId, u32>,
    meta_seen: BTreeSet<PaneId>,
    queued: VecDeque<TaggedCommand>,
    stats: SyncStats,
}

impl SyncEngine {
    /// Creates an engine that will use `marker` for its barrier command.
    ///
    /// `limits.max_response_lines` is also the budget for a `capture-pane`: the
    /// capture asks for at most `max_response_lines - pane_height` history lines,
    /// so the response can never be truncated into "the oldest history pretending
    /// to be the screen".
    #[must_use]
    pub fn new(marker: impl Into<String>, limits: Limits) -> Self {
        Self {
            marker: marker.into(),
            max_capture_lines: limits.max_response_lines,
            started: false,
            finished: false,
            panes: Vec::new(),
            pane_heights: BTreeMap::new(),
            meta_seen: BTreeSet::new(),
            queued: VecDeque::new(),
            stats: SyncStats::default(),
        }
    }

    /// True once the snapshot is complete.
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.finished
    }

    /// Statistics collected so far.
    #[must_use]
    pub const fn stats(&self) -> SyncStats {
        self.stats
    }

    /// The marker command's expected output.
    #[must_use]
    pub fn marker(&self) -> &str {
        &self.marker
    }

    /// The next command to schedule, if the engine is ready for one.
    ///
    /// It returns `None` between a command and its response: the snapshot is a
    /// strictly sequential exchange so that each response stays an unambiguous
    /// watermark.
    pub fn next_command(&mut self) -> Option<TaggedCommand> {
        if self.finished {
            return None;
        }
        if let Some(command) = self.queued.pop_front() {
            return Some(command);
        }
        if self.started {
            return None;
        }
        self.started = true;
        Some(TaggedCommand {
            command: Command::new("display-message")
                .arg("-p")
                .arg(self.marker.clone()),
            tag: Tag::Barrier,
            pane: None,
        })
    }

    /// Applies a snapshot response and advances the sequence.
    ///
    /// # Errors
    ///
    /// Returns [`SyncError`] when the server refused a snapshot command or its
    /// answer cannot be interpreted.  A failed snapshot is reported, never
    /// papered over with a partially filled model.
    pub fn on_response(
        &mut self,
        tag: Tag,
        pane: Option<PaneId>,
        response: &Response,
        model: &mut RemoteModel,
    ) -> Result<SyncOutcome, SyncError> {
        if response.truncated {
            return Err(SyncError::Truncated { tag });
        }
        if !response.ok {
            return Err(SyncError::CommandFailed {
                tag,
                message: response
                    .first_line_lossy()
                    .unwrap_or_else(|| "<no output>".to_string()),
            });
        }
        let mut changes = Vec::new();
        match tag {
            Tag::Barrier => {
                let seen = response.first_line().unwrap_or_default();
                if seen != self.marker.as_bytes() {
                    return Err(SyncError::Malformed {
                        tag,
                        detail: format!(
                            "barrier marker mismatch (got {:?})",
                            String::from_utf8_lossy(seen)
                        ),
                    });
                }
                self.queued.push_back(TaggedCommand {
                    command: Command::new("list-sessions").arg("-F").arg(SESSION_FORMAT),
                    tag: Tag::ListSessions,
                    pane: None,
                });
            }
            Tag::ListSessions => {
                for line in &response.lines {
                    let parsed = parse_session_line(line).ok_or_else(|| SyncError::Malformed {
                        tag,
                        detail: format!("session line {:?}", String::from_utf8_lossy(line)),
                    })?;
                    self.stats.sessions += 1;
                    changes.push(model.upsert_session(parsed.id, parsed.name, parsed.window_count));
                }
                self.queued.push_back(TaggedCommand {
                    command: Command::new("list-windows")
                        .arg("-a")
                        .arg("-F")
                        .arg(WINDOW_FORMAT),
                    tag: Tag::ListWindows,
                    pane: None,
                });
            }
            Tag::ListWindows => {
                for line in &response.lines {
                    let parsed = parse_window_line(line).ok_or_else(|| SyncError::Malformed {
                        tag,
                        detail: format!("window line {:?}", String::from_utf8_lossy(line)),
                    })?;
                    self.stats.windows += 1;
                    changes.extend(model.upsert_window(
                        parsed.id,
                        Some(parsed.session),
                        parsed.index,
                        parsed.active,
                        parsed.name,
                    ));
                    if !parsed.layout.is_empty() {
                        let flags: Vec<Vec<u8>> = if parsed.zoomed {
                            vec![b"zoomed".to_vec()]
                        } else {
                            Vec::new()
                        };
                        match model.set_layout(
                            parsed.id,
                            parsed.layout,
                            parsed.visible_layout,
                            &flags,
                        ) {
                            Ok(updates) => changes.extend(updates),
                            Err(reason) => changes.push(model.mark_desynchronised(format!(
                                "layout of {} not understood: {reason}",
                                parsed.id
                            ))),
                        }
                    }
                }
                self.queued.push_back(TaggedCommand {
                    command: Command::new("list-panes")
                        .arg("-a")
                        .arg("-F")
                        .arg(PANE_FORMAT),
                    tag: Tag::ListPanes,
                    pane: None,
                });
            }
            Tag::ListPanes => {
                self.panes.clear();
                self.pane_heights.clear();
                self.meta_seen.clear();
                for line in &response.lines {
                    let parsed = parse_pane_line(line).ok_or_else(|| SyncError::Malformed {
                        tag,
                        detail: format!("pane line {:?}", String::from_utf8_lossy(line)),
                    })?;
                    self.stats.panes += 1;
                    self.panes.push(parsed.id);
                    self.pane_heights.insert(parsed.id, parsed.height);
                    changes.push(model.upsert_pane(
                        parsed.id,
                        PaneState {
                            window: Some(parsed.window),
                            index: parsed.index,
                            width: parsed.width,
                            height: parsed.height,
                            title: parsed.title,
                            dead: parsed.dead,
                            active: parsed.active,
                            in_mode: parsed.in_mode,
                            ..PaneState::default()
                        },
                    ));
                }
                self.enqueue_pane_steps();
            }
            Tag::CapturePane => {
                let pane = pane.ok_or(SyncError::Malformed {
                    tag,
                    detail: "capture response without a pane".to_string(),
                })?;
                self.stats.captures += 1;
                changes.push(model.mark_pane_ready(pane));
                let outcome = SyncOutcome::PaneCaptured {
                    pane,
                    lines: response.lines.clone(),
                    changes,
                };
                if self.queued.is_empty() && self.meta_seen.len() == self.panes.len() {
                    self.finished = true;
                }
                return Ok(outcome);
            }
            Tag::PaneMeta => {
                let Some(pane) = pane else {
                    return Err(SyncError::Malformed {
                        tag,
                        detail: "metadata response without a pane".to_string(),
                    });
                };
                let first = response.first_line().unwrap_or_default();
                let meta = parse_pane_meta(first).ok_or_else(|| SyncError::Malformed {
                    tag,
                    detail: format!("pane metadata {:?}", String::from_utf8_lossy(first)),
                })?;
                if let Some(state) = model.panes().get(&pane).cloned() {
                    changes.push(model.upsert_pane(
                        pane,
                        PaneState {
                            cursor: meta.cursor,
                            alternate_on: meta.alternate_on,
                            history_size: meta.history_size,
                            history_limit: meta.history_limit,
                            in_mode: meta.in_mode || state.in_mode,
                            mode: meta.mode.or(state.mode),
                            ..state
                        },
                    ));
                }
                self.meta_seen.insert(pane);
            }
        }
        if self.queued.is_empty() && self.meta_seen.len() == self.panes.len() {
            self.finished = true;
            return Ok(SyncOutcome::Done {
                stats: self.stats,
                changes,
            });
        }
        Ok(SyncOutcome::Progress { changes })
    }

    fn enqueue_pane_steps(&mut self) {
        for pane in self.panes.clone() {
            let height = self.pane_heights.get(&pane).copied().unwrap_or(0);
            self.queued.push_back(TaggedCommand {
                command: capture_command(pane, self.capture_history(height)),
                tag: Tag::CapturePane,
                pane: Some(pane),
            });
            self.queued.push_back(TaggedCommand {
                command: meta_command(pane),
                tag: Tag::PaneMeta,
                pane: Some(pane),
            });
        }
    }

    /// How many history lines a pane of `pane_height` rows may ask for without
    /// exceeding the response-line budget.
    ///
    /// A zero budget asks for the visible screen only (`-S 0`); the caller no
    /// longer applies a truncated capture, it fails the snapshot.
    fn capture_history(&self, pane_height: u32) -> u32 {
        self.max_capture_lines.saturating_sub(pane_height)
    }
}

/// Formats used by the snapshot queries.
///
/// A variable-length field (a name or a title) is always **last**, so a line can be
/// split with a bounded split count: names may contain spaces and no separator
/// character is forbidden inside a window name.
pub const SESSION_FORMAT: &str = "#{session_id} #{session_windows} #{session_name}";
/// Window list format: name last.
///
/// `window_visible_layout` is the zoomed layout when the window is zoomed (the
/// normal `window_layout` keeps the real split tree).  `window_zoomed_flag` says
/// which one is on screen.
pub const WINDOW_FORMAT: &str = "#{session_id} #{window_id} #{window_index} \
     #{window_active} #{window_zoomed_flag} #{window_layout} #{window_visible_layout} \
     #{window_name}";
/// Pane list format: title last.
pub const PANE_FORMAT: &str = "#{window_id} #{pane_id} #{pane_index} #{pane_width} #{pane_height} \
     #{pane_active} #{pane_dead} #{pane_in_mode} #{pane_title}";
/// Per-pane metadata query.
pub const PANE_META_FORMAT: &str =
    "#{cursor_x} #{cursor_y} #{alternate_on} #{history_size} #{history_limit} #{pane_in_mode} \
     #{pane_mode} #{pane_dead}";

/// The capture command used for the snapshot.
///
/// `-e` keeps escape sequences (styles, alternate-screen switches and mode
/// changes), `-N` preserves trailing spaces so a row keeps its geometry, `-q` stays
/// quiet about missing panes, and `-S <history>`/`-E -` bounds the capture to the
/// configured response budget.  `history_lines = 0` captures the visible screen
/// only.
///
/// `-J` (join wrapped lines) is deliberately **not** used: it turns physical rows
/// into logical lines, so the last `rows` captured lines are no longer the visible
/// screen, and the history/screen split — the watermark everything else depends on
/// — becomes ambiguous.  The cost is that a wrapped logical line is restored as two
/// physical rows without the wrap flag; `docs/spec/state-sync.md` §5 records that.
#[must_use]
pub fn capture_command(pane: PaneId, history_lines: u32) -> Command {
    let start = if history_lines == 0 {
        "0".to_string()
    } else {
        format!("-{history_lines}")
    };
    Command::new("capture-pane")
        .arg("-p")
        .arg("-e")
        .arg("-q")
        .arg("-N")
        .arg("-S")
        .arg(start)
        .arg("-E")
        .arg("-")
        .arg("-t")
        .arg(pane.to_string())
}

/// The per-pane metadata command.
#[must_use]
pub fn meta_command(pane: PaneId) -> Command {
    Command::new("display-message")
        .arg("-p")
        .arg("-t")
        .arg(pane.to_string())
        .arg(PANE_META_FORMAT)
}

/// A session row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// Session id.
    pub id: SessionId,
    /// Number of windows reported.
    pub window_count: u32,
    /// Session name.
    pub name: Vec<u8>,
}

/// A window row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowRow {
    /// Owning session.
    pub session: SessionId,
    /// Window id.
    pub id: WindowId,
    /// Window index.
    pub index: u32,
    /// True when this is the session's current window.
    pub active: bool,
    /// True when the window is zoomed.
    pub zoomed: bool,
    /// Raw layout string (the real split tree).
    pub layout: Vec<u8>,
    /// Raw visible layout string (a single pane when zoomed).
    pub visible_layout: Vec<u8>,
    /// Window name.
    pub name: Vec<u8>,
}

/// A pane row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    /// Owning window.
    pub window: WindowId,
    /// Pane id.
    pub id: PaneId,
    /// Pane index.
    pub index: u32,
    /// Width in cells.
    pub width: u32,
    /// Height in cells.
    pub height: u32,
    /// True when this is the active pane.
    pub active: bool,
    /// True when the pane's process exited.
    pub dead: bool,
    /// True while in a tmux mode.
    pub in_mode: bool,
    /// Pane title.
    pub title: Vec<u8>,
}

/// Per-pane metadata row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneMeta {
    /// Cursor column and row.
    pub cursor: (u32, u32),
    /// True when the pane is on the alternate screen.
    pub alternate_on: bool,
    /// Lines in history.
    pub history_size: u32,
    /// Configured history limit.
    pub history_limit: u32,
    /// True while in a tmux mode.
    pub in_mode: bool,
    /// Mode name, when reported.
    pub mode: Option<Vec<u8>>,
    /// True when the pane's process exited.
    pub dead: bool,
}

fn split_bounded<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    let mut parts: [&[u8]; N] = [&[]; N];
    let mut rest = line;
    for slot in parts.iter_mut().take(N - 1) {
        let index = rest.iter().position(|b| *b == b' ')?;
        *slot = &rest[..index];
        rest = &rest[index + 1..];
    }
    parts[N - 1] = rest;
    Some(parts)
}

fn parse_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value: u32 = 0;
    for b in bytes {
        value = value.checked_mul(10)?.checked_add(u32::from(*b - b'0'))?;
    }
    Some(value)
}

fn parse_flag(bytes: &[u8]) -> bool {
    bytes == b"1"
}

/// Parses one `list-sessions` line.
#[must_use]
pub fn parse_session_line(line: &[u8]) -> Option<SessionRow> {
    let [id, window_count, name] = split_bounded::<3>(line)?;
    Some(SessionRow {
        id: SessionId::from_wire(id)?,
        window_count: parse_u32(window_count)?,
        name: name.to_vec(),
    })
}

/// Parses one `list-windows` line.
#[must_use]
pub fn parse_window_line(line: &[u8]) -> Option<WindowRow> {
    let [session, id, index, active, zoomed, layout, visible_layout, name] =
        split_bounded::<8>(line)?;
    Some(WindowRow {
        session: SessionId::from_wire(session)?,
        id: WindowId::from_wire(id)?,
        index: parse_u32(index)?,
        active: parse_flag(active),
        zoomed: parse_flag(zoomed),
        layout: layout.to_vec(),
        visible_layout: visible_layout.to_vec(),
        name: name.to_vec(),
    })
}

/// Parses one `list-panes` line.
#[must_use]
pub fn parse_pane_line(line: &[u8]) -> Option<PaneRow> {
    let [window, id, index, width, height, active, dead, in_mode, title] =
        split_bounded::<9>(line)?;
    Some(PaneRow {
        window: WindowId::from_wire(window)?,
        id: PaneId::from_wire(id)?,
        index: parse_u32(index)?,
        width: parse_u32(width)?,
        height: parse_u32(height)?,
        active: parse_flag(active),
        dead: parse_flag(dead),
        in_mode: parse_flag(in_mode),
        title: title.to_vec(),
    })
}

/// Parses one pane metadata line.
#[must_use]
pub fn parse_pane_meta(line: &[u8]) -> Option<PaneMeta> {
    let parts: Vec<&[u8]> = line.split(|b| *b == b' ').collect();
    if parts.len() < 8 {
        return None;
    }
    Some(PaneMeta {
        cursor: (parse_u32(parts[0])?, parse_u32(parts[1])?),
        alternate_on: parse_flag(parts[2]),
        history_size: parse_u32(parts[3])?,
        history_limit: parse_u32(parts[4])?,
        in_mode: parse_flag(parts[5]),
        mode: (!parts[6].is_empty()).then(|| parts[6].to_vec()),
        dead: parse_flag(parts[7]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmux_protocol::limits::Limits;
    use tmux_protocol::parser::ResponseHeader;

    fn response(lines: &[&[u8]]) -> Response {
        Response {
            request: None,
            header: ResponseHeader::default(),
            lines: lines.iter().map(|l| l.to_vec()).collect(),
            ok: true,
            truncated: false,
            closed_by_exit: false,
        }
    }

    fn model() -> RemoteModel {
        RemoteModel::new(crate::epoch::ConnectionEpoch::next(), Limits::default())
    }

    #[test]
    fn parses_session_lines_with_spaces_in_the_name() {
        let row = parse_session_line(b"$0 2 my session").unwrap();
        assert_eq!(row.id, SessionId::new(0));
        assert_eq!(row.window_count, 2);
        assert_eq!(row.name, b"my session");
    }

    #[test]
    fn parses_window_lines() {
        let row = parse_window_line(b"$0 @1 1 1 0 bb62,80x24,0,0,0 bb62,80x24,0,0,0 editor window")
            .unwrap();
        assert_eq!(row.session, SessionId::new(0));
        assert_eq!(row.id, WindowId::new(1));
        assert_eq!(row.index, 1);
        assert!(row.active);
        assert!(!row.zoomed);
        assert_eq!(row.layout, b"bb62,80x24,0,0,0");
        assert_eq!(row.visible_layout, b"bb62,80x24,0,0,0");
        assert_eq!(row.name, b"editor window");
    }

    #[test]
    fn parses_a_zoomed_window_line() {
        let row = parse_window_line(
            b"$0 @1 0 0 1 bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1] bb62,80x24,0,0[80x24,0,0,0] w",
        )
        .unwrap();
        assert!(row.zoomed);
        assert_eq!(row.layout, b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]");
        assert_eq!(row.visible_layout, b"bb62,80x24,0,0[80x24,0,0,0]");
    }

    #[test]
    fn parses_pane_lines() {
        let row = parse_pane_line("@1 %3 1 80 24 1 0 0 中 文 title".as_bytes()).unwrap();
        assert_eq!(row.window, WindowId::new(1));
        assert_eq!(row.id, PaneId::new(3));
        assert_eq!((row.width, row.height), (80, 24));
        assert!(row.active);
        assert!(!row.dead);
        assert_eq!(row.title, "中 文 title".as_bytes());
    }

    #[test]
    fn parses_pane_metadata() {
        let meta = parse_pane_meta(b"12 5 1 300 2000 1 copy-mode 0").unwrap();
        assert_eq!(meta.cursor, (12, 5));
        assert!(meta.alternate_on);
        assert_eq!(meta.history_size, 300);
        assert_eq!(meta.history_limit, 2000);
        assert!(meta.in_mode);
        assert_eq!(meta.mode.as_deref(), Some(&b"copy-mode"[..]));
    }

    #[test]
    fn malformed_rows_are_rejected_not_guessed() {
        assert!(parse_session_line(b"$0 2").is_none());
        assert!(parse_window_line(b"nonsense").is_none());
        assert!(parse_pane_line(b"@1 %3 1 80 24").is_none());
        assert!(parse_pane_meta(b"1 2 3").is_none());
    }

    #[test]
    fn snapshot_runs_in_the_documented_order_and_watermarks_each_pane() {
        let mut engine = SyncEngine::new("__m__", Limits::default());
        let mut model = model();

        let first = engine.next_command().unwrap();
        assert_eq!(first.tag, Tag::Barrier);
        assert!(engine.next_command().is_none(), "one exchange at a time");

        engine
            .on_response(Tag::Barrier, None, &response(&[b"__m__"]), &mut model)
            .unwrap();

        let sessions = engine.next_command().unwrap();
        assert_eq!(sessions.tag, Tag::ListSessions);
        engine
            .on_response(
                Tag::ListSessions,
                None,
                &response(&[b"$0 1 dev"]),
                &mut model,
            )
            .unwrap();

        let windows = engine.next_command().unwrap();
        assert_eq!(windows.tag, Tag::ListWindows);
        engine
            .on_response(
                Tag::ListWindows,
                None,
                &response(&[b"$0 @0 0 1 0 bb62,80x24,0,0,0 bb62,80x24,0,0,0 shell"]),
                &mut model,
            )
            .unwrap();

        assert_eq!(
            model.sessions()[&SessionId::new(0)].windows,
            vec![WindowId::new(0)],
            "the window list must populate the session→window relation"
        );
        assert_eq!(
            model.sessions()[&SessionId::new(0)].current_window,
            Some(WindowId::new(0)),
            "window_active selects the current window"
        );

        let panes = engine.next_command().unwrap();
        assert_eq!(panes.tag, Tag::ListPanes);
        engine
            .on_response(
                Tag::ListPanes,
                None,
                &response(&[b"@0 %0 0 80 24 1 0 0 bash"]),
                &mut model,
            )
            .unwrap();

        let capture = engine.next_command().unwrap();
        assert_eq!(capture.tag, Tag::CapturePane);
        assert_eq!(capture.pane, Some(PaneId::new(0)));
        let encoded = capture
            .command
            .encode(Limits::default())
            .expect("capture encodes");
        assert!(
            String::from_utf8_lossy(&encoded).contains("-S -4072"),
            "a 24-row pane under a 4096-line budget asks for 4072 history lines, got {:?}",
            String::from_utf8_lossy(&encoded)
        );
        let outcome = engine
            .on_response(
                Tag::CapturePane,
                Some(PaneId::new(0)),
                &response(&[b"$ hello", b"world"]),
                &mut model,
            )
            .unwrap();
        match outcome {
            SyncOutcome::PaneCaptured { pane, lines, .. } => {
                assert_eq!(pane, PaneId::new(0));
                assert_eq!(lines.len(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            model.pane_is_ready(PaneId::new(0)),
            "the capture response is the watermark"
        );

        let meta = engine.next_command().unwrap();
        assert_eq!(meta.tag, Tag::PaneMeta);
        engine
            .on_response(
                Tag::PaneMeta,
                Some(PaneId::new(0)),
                &response(&[b"3 1 0 0 2000 0  0"]),
                &mut model,
            )
            .unwrap();

        assert!(engine.is_finished());
        assert!(engine.next_command().is_none());
        assert_eq!(
            engine.stats(),
            SyncStats {
                sessions: 1,
                windows: 1,
                panes: 1,
                captures: 1
            }
        );
    }

    #[test]
    fn a_failed_snapshot_command_is_reported() {
        let mut engine = SyncEngine::new("__m__", Limits::default());
        let mut model = model();
        let _ = engine.next_command();
        let mut bad = response(&[b"no server"]);
        bad.ok = false;
        let err = engine
            .on_response(Tag::Barrier, None, &bad, &mut model)
            .unwrap_err();
        assert!(matches!(err, SyncError::CommandFailed { .. }));
    }

    #[test]
    fn a_wrong_barrier_marker_is_rejected() {
        let mut engine = SyncEngine::new("__m__", Limits::default());
        let mut model = model();
        let _ = engine.next_command();
        let err = engine
            .on_response(Tag::Barrier, None, &response(&[b"__other__"]), &mut model)
            .unwrap_err();
        assert!(matches!(err, SyncError::Malformed { .. }));
    }

    #[test]
    fn capture_command_bounds_history_and_keeps_styles() {
        let bounded = capture_command(PaneId::new(2), 20)
            .encode(Limits::default())
            .unwrap();
        assert_eq!(
            String::from_utf8(bounded).unwrap(),
            "capture-pane -p -e -q -N -S -20 -E - -t %2\n"
        );

        let visible_only = capture_command(PaneId::new(2), 0)
            .encode(Limits::default())
            .unwrap();
        assert_eq!(
            String::from_utf8(visible_only).unwrap(),
            "capture-pane -p -e -q -N -S 0 -E - -t %2\n"
        );
    }

    #[test]
    fn a_truncated_snapshot_response_is_rejected() {
        let mut engine = SyncEngine::new("__m__", Limits::default());
        let mut model = model();
        let mut truncated = response(&[b"old history"]);
        truncated.truncated = true;
        let err = engine
            .on_response(
                Tag::CapturePane,
                Some(PaneId::new(0)),
                &truncated,
                &mut model,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            SyncError::Truncated {
                tag: Tag::CapturePane
            }
        ));
        assert!(
            !model.pane_is_ready(PaneId::new(0)),
            "a truncated capture must never become a pane snapshot"
        );
    }
}
