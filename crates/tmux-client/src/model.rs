//! The remote object model.
//!
//! **The remote state is the source of truth and the GUI is only a projection of
//! it.**  This module therefore never invents structure: every session, window,
//! pane and layout in [`RemoteModel`] is either reported by the server or
//! explicitly marked as not yet known.
//!
//! Local view identity is kept out of this module entirely: a tab belongs to the
//! GUI, a window belongs to tmux, and the two are related but not identical.

use std::collections::BTreeMap;

use tmux_protocol::ids::{PaneId, SessionId, WindowId};
use tmux_protocol::layout::Layout;
use tmux_protocol::{Event, Limits};

use crate::epoch::ConnectionEpoch;

/// Whether the model can be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Consistency {
    /// Built from a complete snapshot and kept up to date by notifications.
    Synced,
    /// Something was lost or truncated; a resynchronisation is required before
    /// the model may be presented as accurate.
    NeedsResync {
        /// Why the model is no longer trustworthy.
        reason: String,
    },
}

impl Consistency {
    /// True when the model may be rendered as accurate.
    #[must_use]
    pub const fn is_synced(&self) -> bool {
        matches!(self, Self::Synced)
    }
}

/// One pane as far as the client knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneState {
    /// Owning window.
    pub window: Option<WindowId>,
    /// Index inside the window.
    pub index: u32,
    /// Width in cells.
    pub width: u32,
    /// Height in cells.
    pub height: u32,
    /// Pane title, untrusted bytes.
    pub title: Vec<u8>,
    /// True when the pane's process has exited.
    pub dead: bool,
    /// True when this is the active pane of its window.
    pub active: bool,
    /// True while the pane is in a tmux mode (copy mode, ...).
    pub in_mode: bool,
    /// Name of that mode when the server reports one.
    pub mode: Option<Vec<u8>>,
    /// Cursor position inside the pane.
    pub cursor: (u32, u32),
    /// True when the pane is on the alternate screen.
    pub alternate_on: bool,
    /// Number of lines currently in the history.
    pub history_size: u32,
    /// Configured history limit.
    pub history_limit: u32,
    /// False until the pane's screen snapshot has been loaded.
    ///
    /// Output that arrives before the snapshot is dropped on purpose: it is
    /// already contained in the snapshot (see `docs/spec/state-sync.md`).
    pub snapshot_loaded: bool,
    /// Snapshot generation this object was last seen in (internal bookkeeping).
    pub(crate) seen: u64,
}

/// One window as far as the client knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowState {
    /// Sessions the window is linked to.
    ///
    /// A tmux window may be linked into several sessions; keeping the set here
    /// avoids silently picking whichever `list-windows -a` row arrived last.
    pub sessions: Vec<SessionId>,
    /// Window index inside the session.
    pub index: u32,
    /// Window name, untrusted bytes.
    pub name: Vec<u8>,
    /// Parsed layout, when a layout string has been seen and parsed.
    pub layout: Option<Layout>,
    /// Raw layout string as last reported (kept even when parsing failed).
    pub layout_raw: Vec<u8>,
    /// The layout actually visible, when tmux reported a different one (zoom).
    pub visible_layout: Option<Layout>,
    /// Raw visible layout string.
    pub visible_layout_raw: Vec<u8>,
    /// True while the window is zoomed (one pane fills the window).
    pub zoomed: bool,
    /// Panes in this window.
    pub panes: Vec<PaneId>,
    /// Active pane of the window.
    pub active_pane: Option<PaneId>,
    /// Snapshot generation this object was last seen in (internal bookkeeping).
    pub(crate) seen: u64,
}

/// One session as far as the client knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionState {
    /// Session name, untrusted bytes.
    pub name: Vec<u8>,
    /// Number of windows tmux reports for the session.
    pub window_count: u32,
    /// Windows linked to the session, in index order.
    pub windows: Vec<WindowId>,
    /// Current window.
    pub current_window: Option<WindowId>,
    /// Snapshot generation this object was last seen in (internal bookkeeping).
    pub(crate) seen: u64,
}

/// A change the model produced, for a UI or a test to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelChange {
    /// A session appeared or was refreshed.
    SessionUpdated(SessionId),
    /// A session disappeared.
    SessionRemoved(SessionId),
    /// A window appeared or changed.
    WindowUpdated(WindowId),
    /// A window disappeared.
    WindowRemoved(WindowId),
    /// A pane appeared or changed.
    PaneUpdated(PaneId),
    /// A pane that the model had never seen appeared in a layout.
    ///
    /// The session answers this by scheduling a capture for that pane; the model
    /// has no snapshot for it yet, so its output is dropped until then.
    PaneDiscovered(PaneId),
    /// A pane's geometry changed (the terminal core must be resized).
    PaneResized {
        /// Affected pane.
        pane: PaneId,
        /// New width in cells.
        columns: u16,
        /// New height in cells.
        rows: u16,
    },
    /// A pane disappeared.
    PaneRemoved(PaneId),
    /// The model became trustworthy.
    Synced,
    /// The model can no longer be trusted.
    Desynchronised {
        /// Why.
        reason: String,
    },
}

/// The client's picture of the remote server.
#[derive(Debug, Clone)]
pub struct RemoteModel {
    epoch: ConnectionEpoch,
    limits: Limits,
    consistency: Consistency,
    /// Session currently attached, as reported by `%session-changed`.
    attached: Option<SessionId>,
    sessions: BTreeMap<SessionId, SessionState>,
    windows: BTreeMap<WindowId, WindowState>,
    panes: BTreeMap<PaneId, PaneState>,
    /// Output bytes dropped because their pane had no snapshot yet.
    dropped_output_bytes: u64,
    /// Monotonic identifier of the snapshot currently being applied.
    snapshot_generation: u64,
    /// Generation in which the last desynchronisation happened.
    desync_generation: u64,
}

impl RemoteModel {
    /// Creates an empty model for a connection.
    #[must_use]
    pub fn new(epoch: ConnectionEpoch, limits: Limits) -> Self {
        Self {
            epoch,
            limits,
            consistency: Consistency::NeedsResync {
                reason: "no snapshot loaded yet".to_string(),
            },
            attached: None,
            sessions: BTreeMap::new(),
            windows: BTreeMap::new(),
            panes: BTreeMap::new(),
            dropped_output_bytes: 0,
            snapshot_generation: 1,
            desync_generation: 0,
        }
    }

    /// Starts a snapshot (initial attach or a resynchronisation).
    ///
    /// Every pane becomes "not ready" again: output that arrives while its new
    /// capture is in flight is already contained in that capture and must be
    /// dropped.  Session→window links are cleared and rebuilt by the snapshot so a
    /// window that disappeared while we were disconnected cannot survive.
    pub fn begin_snapshot(&mut self) {
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
        self.consistency = Consistency::NeedsResync {
            reason: "snapshot in progress".to_string(),
        };
        for pane in self.panes.values_mut() {
            pane.snapshot_loaded = false;
            pane.seen = 0;
        }
        for session in self.sessions.values_mut() {
            session.windows.clear();
            session.current_window = None;
            session.seen = 0;
        }
        for window in self.windows.values_mut() {
            window.sessions.clear();
            window.seen = 0;
        }
    }

    /// The connection this model belongs to.
    #[must_use]
    pub const fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// Whether the model may be presented as accurate.
    #[must_use]
    pub const fn consistency(&self) -> &Consistency {
        &self.consistency
    }

    /// Number of output bytes dropped because their pane had no snapshot yet.
    ///
    /// This is deliberately observable: silently discarding output is exactly the
    /// failure this project must not hide.
    #[must_use]
    pub const fn dropped_output_bytes(&self) -> u64 {
        self.dropped_output_bytes
    }

    /// Sessions known to the client.
    #[must_use]
    pub const fn sessions(&self) -> &BTreeMap<SessionId, SessionState> {
        &self.sessions
    }

    /// Windows known to the client.
    #[must_use]
    pub const fn windows(&self) -> &BTreeMap<WindowId, WindowState> {
        &self.windows
    }

    /// Panes known to the client.
    #[must_use]
    pub const fn panes(&self) -> &BTreeMap<PaneId, PaneState> {
        &self.panes
    }

    /// The attached session, if known.
    #[must_use]
    pub const fn attached(&self) -> Option<SessionId> {
        self.attached
    }

    /// Marks the model as complete.
    ///
    /// Returns `None` when anything marked the model desynchronised **during this
    /// snapshot**: a truncated capture or a protocol desync must survive to the
    /// caller instead of being overwritten by a later `Done` from the sequence.
    pub fn mark_synced(&mut self) -> Option<ModelChange> {
        if self.desync_generation == self.snapshot_generation {
            return None;
        }
        self.consistency = Consistency::Synced;
        Some(ModelChange::Synced)
    }

    /// Marks the model as untrustworthy, keeping what is displayed but flagging it.
    pub fn mark_desynchronised(&mut self, reason: impl Into<String>) -> ModelChange {
        let reason = reason.into();
        self.desync_generation = self.snapshot_generation;
        self.consistency = Consistency::NeedsResync {
            reason: reason.clone(),
        };
        ModelChange::Desynchronised { reason }
    }

    /// Removes objects that the completed snapshot did not report.
    ///
    /// Only call this after [`RemoteModel::mark_synced`] returned `Some`: a
    /// partial snapshot must never delete what it simply did not reach.
    pub fn prune_unseen(&mut self) -> Vec<ModelChange> {
        let generation = self.snapshot_generation;
        let mut changes = Vec::new();

        let stale_windows: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, window)| window.seen != generation)
            .map(|(id, _)| *id)
            .collect();
        for window in stale_windows {
            self.windows.remove(&window);
            changes.push(ModelChange::WindowRemoved(window));
        }

        let orphan_panes: Vec<PaneId> = self
            .panes
            .iter()
            .filter(|(_, pane)| {
                pane.seen != generation
                    || pane
                        .window
                        .is_some_and(|window| !self.windows.contains_key(&window))
            })
            .map(|(id, _)| *id)
            .collect();
        for pane in orphan_panes {
            self.panes.remove(&pane);
            changes.push(ModelChange::PaneRemoved(pane));
        }

        let stale_sessions: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.seen != generation)
            .map(|(id, _)| *id)
            .collect();
        for session in stale_sessions {
            self.sessions.remove(&session);
            changes.push(ModelChange::SessionRemoved(session));
            if self.attached == Some(session) {
                self.attached = None;
            }
        }

        changes
    }

    /// Inserts or replaces a session discovered by a snapshot query.
    pub fn upsert_session(
        &mut self,
        id: SessionId,
        name: Vec<u8>,
        window_count: u32,
    ) -> ModelChange {
        let generation = self.snapshot_generation;
        let entry = self.sessions.entry(id).or_default();
        entry.name = name;
        entry.window_count = window_count;
        entry.seen = generation;
        ModelChange::SessionUpdated(id)
    }

    /// Inserts or replaces a window discovered by a snapshot query.
    ///
    /// The window is linked into `session` (at most once) and, when `active` is
    /// set, becomes that session's current window.  This is what gives the GUI a
    /// usable session→window list instead of a flat window map.
    pub fn upsert_window(
        &mut self,
        id: WindowId,
        session: Option<SessionId>,
        index: u32,
        active: bool,
        name: Vec<u8>,
    ) -> Vec<ModelChange> {
        let generation = self.snapshot_generation;
        let mut changes = Vec::new();
        {
            let entry = self.windows.entry(id).or_default();
            if let Some(session) = session {
                if !entry.sessions.contains(&session) {
                    entry.sessions.push(session);
                }
            }
            entry.index = index;
            entry.name = name;
            entry.seen = generation;
        }
        changes.push(ModelChange::WindowUpdated(id));
        if let Some(session) = session {
            self.link_window(session, id, index, active);
            changes.push(ModelChange::SessionUpdated(session));
        }
        changes
    }

    fn link_window(&mut self, session: SessionId, window: WindowId, index: u32, active: bool) {
        let generation = self.snapshot_generation;
        let entry = self.sessions.entry(session).or_default();
        entry.seen = generation;
        if !entry.windows.contains(&window) {
            entry.windows.push(window);
        }
        // Keep the list in tmux's window-index order.  `index` is the index in
        // *this* session; linked windows can have a different index in another
        // session, so re-sort from the per-session value we have just recorded.
        entry.windows.sort_by_key(|candidate| {
            if *candidate == window {
                index
            } else {
                self.windows
                    .get(candidate)
                    .map_or(u32::MAX, |state| state.index)
            }
        });
        if active {
            entry.current_window = Some(window);
        }
    }

    /// Inserts or replaces a pane discovered by a snapshot query.
    pub fn upsert_pane(&mut self, id: PaneId, state: PaneState) -> ModelChange {
        let generation = self.snapshot_generation;
        let window = state.window;
        let mut state = state;
        state.seen = generation;
        self.panes.insert(id, state);
        if let Some(window) = window {
            // A pane belongs to one window; do not leave it in a stale list after
            // a move.
            for (other, entry) in &mut self.windows {
                if *other != window {
                    entry.panes.retain(|pane| *pane != id);
                }
            }
            let entry = self.windows.entry(window).or_default();
            if !entry.panes.contains(&id) {
                entry.panes.push(id);
            }
            let panes = &self.panes;
            entry
                .panes
                .sort_by_key(|pane| panes.get(pane).map_or(0, |state| state.index));
        }
        ModelChange::PaneUpdated(id)
    }

    /// Records a parsed layout for a window.
    ///
    /// The layout is also the authoritative source of pane geometry after a
    /// resize or a split: existing panes get [`ModelChange::PaneResized`] when
    /// their size changed, and panes the model has never seen get
    /// [`ModelChange::PaneDiscovered`] so the session can capture them.
    ///
    /// # Errors
    ///
    /// Returns the parse error when either layout string (normal or visible) is
    /// malformed.  The caller marks the model desynchronised.
    pub fn set_layout(
        &mut self,
        window: WindowId,
        raw: Vec<u8>,
        visible_raw: Vec<u8>,
        flags: &[Vec<u8>],
    ) -> Result<Vec<ModelChange>, String> {
        let parsed = Layout::parse(&raw).map_err(|err| err.to_string())?;
        let visible = if visible_raw.is_empty() {
            None
        } else {
            Some(Layout::parse(&visible_raw).map_err(|err| format!("visible layout: {err}"))?)
        };
        let generation = self.snapshot_generation;
        let zoomed = flags.iter().any(|flag| flag == b"zoomed");
        let mut changes = Vec::new();

        {
            let entry = self.windows.entry(window).or_default();
            entry.layout_raw = raw;
            entry.layout = Some(parsed.clone());
            entry.visible_layout_raw = visible_raw;
            entry.visible_layout = visible;
            entry.zoomed = zoomed;
            entry.seen = generation;
        }

        let panes = parsed.panes();
        let mut ids = Vec::with_capacity(panes.len());
        for (position, (pane, cell)) in panes.iter().enumerate() {
            ids.push(*pane);
            let columns = u16::try_from(cell.width).unwrap_or(u16::MAX);
            let rows = u16::try_from(cell.height).unwrap_or(u16::MAX);
            match self.panes.get_mut(pane) {
                Some(state) => {
                    state.seen = generation;
                    state.window = Some(window);
                    if state.width != cell.width || state.height != cell.height {
                        state.width = cell.width;
                        state.height = cell.height;
                        changes.push(ModelChange::PaneResized {
                            pane: *pane,
                            columns,
                            rows,
                        });
                    }
                }
                None => {
                    self.panes.insert(
                        *pane,
                        PaneState {
                            window: Some(window),
                            index: u32::try_from(position).unwrap_or(u32::MAX),
                            width: cell.width,
                            height: cell.height,
                            seen: generation,
                            ..PaneState::default()
                        },
                    );
                    changes.push(ModelChange::PaneDiscovered(*pane));
                }
            }
        }
        self.windows.entry(window).or_default().panes = ids;
        changes.push(ModelChange::WindowUpdated(window));
        Ok(changes)
    }

    /// True when the pane's snapshot has been loaded, so its output may be applied.
    #[must_use]
    pub fn pane_is_ready(&self, pane: PaneId) -> bool {
        self.panes
            .get(&pane)
            .is_some_and(|state| state.snapshot_loaded)
    }

    /// Marks a pane's snapshot as loaded.
    pub fn mark_pane_ready(&mut self, pane: PaneId) -> ModelChange {
        let generation = self.snapshot_generation;
        let entry = self.panes.entry(pane).or_default();
        entry.snapshot_loaded = true;
        entry.seen = generation;
        ModelChange::PaneUpdated(pane)
    }

    /// Counts output bytes that were dropped because the pane was not ready.
    pub fn note_dropped_output(&mut self, bytes: usize) {
        self.dropped_output_bytes = self.dropped_output_bytes.saturating_add(bytes as u64);
    }

    /// Applies one parser event, returning the changes it caused.
    ///
    /// The caller decides what to do with [`Event::Output`]: the model only
    /// answers whether the pane is ready to receive bytes.
    pub fn apply(&mut self, event: &Event) -> Vec<ModelChange> {
        let generation = self.snapshot_generation;
        let mut changes = Vec::new();
        match event {
            Event::WindowAdd { window } | Event::UnlinkedWindowAdd { window } => {
                self.windows.entry(*window).or_default().seen = generation;
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::WindowClose { window } | Event::UnlinkedWindowClose { window } => {
                let mut removed: Vec<PaneId> = Vec::new();
                if let Some(state) = self.windows.remove(window) {
                    removed.extend(state.panes);
                }
                // The pane list can lag behind a move or a missed layout change,
                // so also remove every pane whose owner is this window.
                removed.extend(
                    self.panes
                        .iter()
                        .filter(|(_, state)| state.window == Some(*window))
                        .map(|(pane, _)| *pane),
                );
                removed.sort_unstable();
                removed.dedup();
                for pane in removed {
                    self.panes.remove(&pane);
                    changes.push(ModelChange::PaneRemoved(pane));
                }
                for (session_id, session) in &mut self.sessions {
                    let had = session.windows.len();
                    session.windows.retain(|candidate| candidate != window);
                    let cleared = session.current_window == Some(*window);
                    if cleared {
                        session.current_window = None;
                    }
                    if had != session.windows.len() || cleared {
                        changes.push(ModelChange::SessionUpdated(*session_id));
                    }
                }
                changes.push(ModelChange::WindowRemoved(*window));
            }
            Event::WindowRenamed { window, name }
            | Event::UnlinkedWindowRenamed { window, name } => {
                let entry = self.windows.entry(*window).or_default();
                entry.name.clone_from(name);
                entry.seen = generation;
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::WindowPaneChanged { window, pane }
            | Event::UnlinkedWindowPaneChanged { window, pane } => {
                let panes = self
                    .windows
                    .get(window)
                    .map(|state| state.panes.clone())
                    .unwrap_or_default();
                for candidate in panes {
                    if let Some(state) = self.panes.get_mut(&candidate) {
                        state.active = candidate == *pane;
                    }
                }
                let entry = self.windows.entry(*window).or_default();
                entry.active_pane = Some(*pane);
                entry.seen = generation;
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::LayoutChange {
                window,
                layout,
                visible_layout,
                flags,
            } => {
                match self.set_layout(*window, layout.clone(), visible_layout.clone(), flags) {
                    Ok(updates) => changes.extend(updates),
                    Err(reason) => {
                        changes.push(self.mark_desynchronised(format!(
                            "unparsable layout for {window}: {reason}"
                        )));
                    }
                }
            }
            Event::SessionChanged { session, name } => {
                let entry = self.sessions.entry(*session).or_default();
                entry.name.clone_from(name);
                entry.seen = generation;
                self.attached = Some(*session);
                changes.push(ModelChange::SessionUpdated(*session));
            }
            Event::SessionRenamed { session, name } => {
                // Renaming another session must not move the "attached" pointer.
                let entry = self.sessions.entry(*session).or_default();
                entry.name.clone_from(name);
                entry.seen = generation;
                changes.push(ModelChange::SessionUpdated(*session));
            }
            Event::SessionWindowChanged { session, window } => {
                let entry = self.sessions.entry(*session).or_default();
                entry.current_window = Some(*window);
                entry.seen = generation;
                changes.push(ModelChange::SessionUpdated(*session));
            }
            Event::SessionsChanged => {
                // The set of sessions changed and the notification carries no
                // details: only a fresh query can restore accuracy.
                changes.push(self.mark_desynchronised("sessions changed"));
            }
            Event::PaneModeChanged { pane, mode } => {
                let entry = self.panes.entry(*pane).or_default();
                if let Some(mode) = mode {
                    entry.in_mode = true;
                    entry.mode = Some(mode.clone());
                } else {
                    // A bare notification means "the mode changed", not "left the
                    // mode": tmux 3.4 has no mode argument on enter *or* leave,
                    // while 3.5+ sends the name on enter and a bare line on leave.
                    // Toggling is correct for both shapes; treating absence as
                    // `false` was not.
                    entry.in_mode = !entry.in_mode;
                    entry.mode = None;
                }
                entry.seen = generation;
                changes.push(ModelChange::PaneUpdated(*pane));
            }
            Event::Desync { reason, .. } => {
                changes.push(
                    self.mark_desynchronised(format!("protocol desync: {}", reason.as_str())),
                );
            }
            Event::Exit { reason } => {
                let detail = reason.as_ref().map_or_else(
                    || "control mode exited".to_string(),
                    |r| format!("control mode exited: {}", String::from_utf8_lossy(r)),
                );
                changes.push(self.mark_desynchronised(detail));
            }
            Event::ResponseBegin(_)
            | Event::ResponseLine(_)
            | Event::ResponseEnd(_)
            | Event::ResponseError(_)
            | Event::Output { .. }
            | Event::ExtendedOutput { .. }
            | Event::Pause { .. }
            | Event::Continue { .. }
            | Event::ClientSessionChanged { .. }
            | Event::ClientDetached { .. }
            | Event::PasteBufferChanged { .. }
            | Event::PasteBufferDeleted { .. }
            | Event::SubscriptionChanged { .. }
            | Event::Message { .. }
            | Event::ConfigError { .. }
            | Event::Unknown { .. }
            | Event::Raw { .. } => {}
        }
        changes
    }

    /// The limits this model was built with.
    #[must_use]
    pub const fn limits(&self) -> Limits {
        self.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tmux_protocol::parser::ResponseHeader;

    fn model() -> RemoteModel {
        RemoteModel::new(ConnectionEpoch::next(), Limits::default())
    }

    #[test]
    fn starts_untrusted_and_becomes_trusted_only_explicitly() {
        let mut model = model();
        assert!(!model.consistency().is_synced());
        assert_eq!(model.mark_synced(), Some(ModelChange::Synced));
        assert!(model.consistency().is_synced());
    }

    #[test]
    fn a_desync_during_a_snapshot_survives_mark_synced() {
        let mut model = model();
        model.begin_snapshot();
        model.mark_desynchronised("truncated capture");
        assert_eq!(
            model.mark_synced(),
            None,
            "Done must not paper over a desync"
        );
        assert!(!model.consistency().is_synced());

        // A new snapshot generation is clean again.
        model.begin_snapshot();
        assert_eq!(model.mark_synced(), Some(ModelChange::Synced));
    }

    #[test]
    fn begin_snapshot_makes_panes_wait_for_their_new_capture() {
        let mut model = model();
        let pane = PaneId::new(2);
        model.upsert_pane(pane, PaneState::default());
        model.mark_pane_ready(pane);
        assert!(model.pane_is_ready(pane));
        model.begin_snapshot();
        assert!(!model.pane_is_ready(pane));
    }

    #[test]
    fn output_is_refused_until_the_pane_snapshot_is_loaded() {
        let mut model = model();
        let pane = PaneId::new(1);
        model.upsert_pane(pane, PaneState::default());
        assert!(!model.pane_is_ready(pane));
        model.mark_pane_ready(pane);
        assert!(model.pane_is_ready(pane));
    }

    #[test]
    fn layout_change_updates_panes_and_is_parsed() {
        let mut model = model();
        let window = WindowId::new(0);
        let changes = model.apply(&Event::LayoutChange {
            window,
            layout: b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]".to_vec(),
            visible_layout: Vec::new(),
            flags: Vec::new(),
        });
        assert!(changes.contains(&ModelChange::WindowUpdated(window)));
        assert!(changes.contains(&ModelChange::PaneDiscovered(PaneId::new(0))));
        assert!(changes.contains(&ModelChange::PaneDiscovered(PaneId::new(1))));
        let state = &model.windows()[&window];
        assert_eq!(state.panes.len(), 2);
        assert_eq!(state.layout.as_ref().unwrap().root.pane_count(), 2);
        assert_eq!(model.panes()[&PaneId::new(0)].width, 80);
        assert_eq!(model.panes()[&PaneId::new(0)].height, 12);
    }

    #[test]
    fn layout_change_reports_new_pane_and_resize() {
        let mut model = model();
        let window = WindowId::new(0);
        let _ = model.apply(&Event::LayoutChange {
            window,
            layout: b"bb62,80x24,0,0,0".to_vec(),
            visible_layout: Vec::new(),
            flags: Vec::new(),
        });
        let changes = model.apply(&Event::LayoutChange {
            window,
            layout: b"bb62,100x30,0,0[50x30,0,0,0,49x30,51,0,1]".to_vec(),
            visible_layout: Vec::new(),
            flags: Vec::new(),
        });
        assert!(changes.contains(&ModelChange::PaneResized {
            pane: PaneId::new(0),
            columns: 50,
            rows: 30
        }));
        assert!(changes.contains(&ModelChange::PaneDiscovered(PaneId::new(1))));
        assert_eq!(model.panes()[&PaneId::new(0)].width, 50);
    }

    #[test]
    fn zoomed_visible_layout_is_recorded_separately() {
        let mut model = model();
        let window = WindowId::new(0);
        let _ = model.apply(&Event::LayoutChange {
            window,
            layout: b"bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]".to_vec(),
            visible_layout: b"bb62,80x24,0,0,0".to_vec(),
            flags: vec![b"zoomed".to_vec()],
        });
        let state = &model.windows()[&window];
        assert!(state.zoomed);
        assert_eq!(state.visible_layout.as_ref().unwrap().root.pane_count(), 1);
        assert_eq!(state.layout.as_ref().unwrap().root.pane_count(), 2);
    }

    #[test]
    fn an_unparsable_layout_desynchronises_instead_of_guessing() {
        let mut model = model();
        let changes = model.apply(&Event::LayoutChange {
            window: WindowId::new(0),
            layout: b"garbage".to_vec(),
            visible_layout: Vec::new(),
            flags: Vec::new(),
        });
        assert!(matches!(
            changes.last(),
            Some(ModelChange::Desynchronised { .. })
        ));
        assert!(!model.consistency().is_synced());
    }

    #[test]
    fn sessions_changed_forces_a_resync() {
        let mut model = model();
        let _ = model.mark_synced();
        model.apply(&Event::SessionsChanged);
        assert!(!model.consistency().is_synced());
    }

    #[test]
    fn closing_a_window_removes_its_panes() {
        let mut model = model();
        let window = WindowId::new(3);
        let pane = PaneId::new(7);
        let _ = model.upsert_window(window, None, 0, false, b"w".to_vec());
        model.upsert_pane(
            pane,
            PaneState {
                window: Some(window),
                ..PaneState::default()
            },
        );
        let changes = model.apply(&Event::WindowClose { window });
        assert!(changes.contains(&ModelChange::PaneRemoved(pane)));
        assert!(model.panes().is_empty());
    }

    #[test]
    fn session_renamed_does_not_move_the_attached_pointer() {
        let mut model = model();
        model.apply(&Event::SessionChanged {
            session: SessionId::new(0),
            name: b"first".to_vec(),
        });
        assert_eq!(model.attached(), Some(SessionId::new(0)));
        model.apply(&Event::SessionRenamed {
            session: SessionId::new(1),
            name: b"other".to_vec(),
        });
        assert_eq!(
            model.attached(),
            Some(SessionId::new(0)),
            "renaming another session must not attach this client to it"
        );
    }

    #[test]
    fn a_bare_pane_mode_notification_toggles() {
        let mut model = model();
        let pane = PaneId::new(0);
        model.upsert_pane(pane, PaneState::default());
        model.apply(&Event::PaneModeChanged { pane, mode: None });
        assert!(model.panes()[&pane].in_mode, "entering copy mode");
        model.apply(&Event::PaneModeChanged {
            pane,
            mode: Some(b"copy-mode".to_vec()),
        });
        assert!(model.panes()[&pane].in_mode);
        model.apply(&Event::PaneModeChanged { pane, mode: None });
        assert!(!model.panes()[&pane].in_mode, "leaving copy mode");
    }

    #[test]
    fn upsert_window_links_the_session_and_marks_the_current_window() {
        let mut model = model();
        let session = SessionId::new(0);
        let first = WindowId::new(0);
        let second = WindowId::new(1);
        model.upsert_session(session, b"dev".to_vec(), 2);
        let _ = model.upsert_window(first, Some(session), 0, false, b"one".to_vec());
        let _ = model.upsert_window(second, Some(session), 1, true, b"two".to_vec());
        let state = &model.sessions()[&session];
        assert_eq!(state.window_count, 2);
        assert_eq!(state.windows, vec![first, second]);
        assert_eq!(state.current_window, Some(second));
        assert_eq!(model.windows()[&first].sessions, vec![session]);
    }

    #[test]
    fn prune_unseen_removes_objects_the_snapshot_did_not_report() {
        let mut model = model();
        let session = SessionId::new(0);
        let window = WindowId::new(0);
        let pane = PaneId::new(0);
        model.begin_snapshot();
        model.upsert_session(session, b"dev".to_vec(), 1);
        let _ = model.upsert_window(window, Some(session), 0, true, b"w".to_vec());
        model.upsert_pane(
            pane,
            PaneState {
                window: Some(window),
                ..PaneState::default()
            },
        );
        assert_eq!(model.mark_synced(), Some(ModelChange::Synced));
        assert!(model.prune_unseen().is_empty(), "everything was seen");

        // A second snapshot reports only a different session: the old objects are
        // removed at the end of it.
        model.begin_snapshot();
        let other = SessionId::new(1);
        model.upsert_session(other, b"other".to_vec(), 0);
        assert_eq!(model.mark_synced(), Some(ModelChange::Synced));
        let changes = model.prune_unseen();
        assert!(changes.contains(&ModelChange::SessionRemoved(session)));
        assert!(changes.contains(&ModelChange::WindowRemoved(window)));
        assert!(changes.contains(&ModelChange::PaneRemoved(pane)));
        assert!(!model.sessions().contains_key(&session));
    }

    #[test]
    fn desync_events_are_never_ignored() {
        let mut model = model();
        let _ = model.mark_synced();
        model.apply(&Event::Desync {
            reason: tmux_protocol::parser::Desync::LineTooLong,
            dropped_bytes: 4096,
        });
        assert!(!model.consistency().is_synced());
    }

    #[test]
    fn responses_do_not_change_the_model() {
        let mut model = model();
        let changes = model.apply(&Event::ResponseBegin(ResponseHeader::default()));
        assert!(changes.is_empty());
    }
}
