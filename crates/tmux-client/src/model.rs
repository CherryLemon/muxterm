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
}

/// One window as far as the client knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WindowState {
    /// Owning session, when the window is linked to one the client tracks.
    pub session: Option<SessionId>,
    /// Window index inside the session.
    pub index: u32,
    /// Window name, untrusted bytes.
    pub name: Vec<u8>,
    /// Parsed layout, when a layout string has been seen and parsed.
    pub layout: Option<Layout>,
    /// Raw layout string as last reported (kept even when parsing failed).
    pub layout_raw: Vec<u8>,
    /// Panes in this window.
    pub panes: Vec<PaneId>,
    /// Active pane of the window.
    pub active_pane: Option<PaneId>,
}

/// One session as far as the client knows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionState {
    /// Session name, untrusted bytes.
    pub name: Vec<u8>,
    /// Windows linked to the session, in index order.
    pub windows: Vec<WindowId>,
    /// Current window.
    pub current_window: Option<WindowId>,
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
    /// Panes whose output may be applied to a terminal emulator.
    dropped_output_bytes: u64,
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
    pub fn mark_synced(&mut self) -> ModelChange {
        self.consistency = Consistency::Synced;
        ModelChange::Synced
    }

    /// Marks the model as untrustworthy, keeping what is displayed but flagging it.
    pub fn mark_desynchronised(&mut self, reason: impl Into<String>) -> ModelChange {
        let reason = reason.into();
        self.consistency = Consistency::NeedsResync {
            reason: reason.clone(),
        };
        ModelChange::Desynchronised { reason }
    }

    /// Inserts or replaces a session discovered by a snapshot query.
    pub fn upsert_session(
        &mut self,
        id: SessionId,
        name: Vec<u8>,
        windows: Vec<WindowId>,
        current_window: Option<WindowId>,
    ) -> ModelChange {
        let entry = self.sessions.entry(id).or_default();
        entry.name = name;
        entry.windows = windows;
        entry.current_window = current_window;
        ModelChange::SessionUpdated(id)
    }

    /// Inserts or replaces a window discovered by a snapshot query.
    pub fn upsert_window(
        &mut self,
        id: WindowId,
        session: Option<SessionId>,
        index: u32,
        name: Vec<u8>,
    ) -> ModelChange {
        let entry = self.windows.entry(id).or_default();
        entry.session = session;
        entry.index = index;
        entry.name = name;
        ModelChange::WindowUpdated(id)
    }

    /// Inserts or replaces a pane discovered by a snapshot query.
    pub fn upsert_pane(&mut self, id: PaneId, state: PaneState) -> ModelChange {
        let window = state.window;
        self.panes.insert(id, state);
        if let Some(window) = window {
            let entry = self.windows.entry(window).or_default();
            if !entry.panes.contains(&id) {
                entry.panes.push(id);
                entry
                    .panes
                    .sort_by_key(|pane| self.panes.get(pane).map_or(0, |state| state.index));
            }
        }
        ModelChange::PaneUpdated(id)
    }

    /// Records a parsed layout for a window.
    pub fn set_layout(&mut self, window: WindowId, raw: Vec<u8>) -> Result<ModelChange, String> {
        let parsed = Layout::parse(&raw).map_err(|err| err.to_string())?;
        let entry = self.windows.entry(window).or_default();
        entry.layout_raw = raw;
        let panes: Vec<PaneId> = parsed.panes().into_iter().map(|(pane, _)| pane).collect();
        entry.panes = panes;
        entry.layout = Some(parsed);
        Ok(ModelChange::WindowUpdated(window))
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
        let entry = self.panes.entry(pane).or_default();
        entry.snapshot_loaded = true;
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
        let mut changes = Vec::new();
        match event {
            Event::WindowAdd { window } | Event::UnlinkedWindowAdd { window } => {
                self.windows.entry(*window).or_default();
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::WindowClose { window } | Event::UnlinkedWindowClose { window } => {
                if let Some(state) = self.windows.remove(window) {
                    for pane in &state.panes {
                        self.panes.remove(pane);
                        changes.push(ModelChange::PaneRemoved(*pane));
                    }
                }
                changes.push(ModelChange::WindowRemoved(*window));
            }
            Event::WindowRenamed { window, name }
            | Event::UnlinkedWindowRenamed { window, name } => {
                let entry = self.windows.entry(*window).or_default();
                entry.name.clone_from(name);
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::WindowPaneChanged { window, pane }
            | Event::UnlinkedWindowPaneChanged { window, pane } => {
                self.windows.entry(*window).or_default().active_pane = Some(*pane);
                changes.push(ModelChange::WindowUpdated(*window));
            }
            Event::LayoutChange { window, layout, .. } => {
                match self.set_layout(*window, layout.clone()) {
                    Ok(change) => changes.push(change),
                    Err(reason) => {
                        changes.push(self.mark_desynchronised(format!(
                            "unparsable layout for {window}: {reason}"
                        )));
                    }
                }
            }
            Event::SessionChanged { session, name } | Event::SessionRenamed { session, name } => {
                let entry = self.sessions.entry(*session).or_default();
                entry.name.clone_from(name);
                self.attached = Some(*session);
                changes.push(ModelChange::SessionUpdated(*session));
            }
            Event::SessionWindowChanged { session, window } => {
                self.sessions.entry(*session).or_default().current_window = Some(*window);
                changes.push(ModelChange::SessionUpdated(*session));
            }
            Event::SessionsChanged => {
                // The set of sessions changed and the notification carries no
                // details: only a fresh query can restore accuracy.
                changes.push(self.mark_desynchronised("sessions changed"));
            }
            Event::PaneModeChanged { pane, mode } => {
                let entry = self.panes.entry(*pane).or_default();
                entry.in_mode = mode.is_some();
                entry.mode.clone_from(mode);
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
        assert_eq!(model.mark_synced(), ModelChange::Synced);
        assert!(model.consistency().is_synced());
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
        assert_eq!(changes, vec![ModelChange::WindowUpdated(window)]);
        let state = &model.windows()[&window];
        assert_eq!(state.panes.len(), 2);
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
        model.mark_synced();
        model.apply(&Event::SessionsChanged);
        assert!(!model.consistency().is_synced());
    }

    #[test]
    fn closing_a_window_removes_its_panes() {
        let mut model = model();
        let window = WindowId::new(3);
        let pane = PaneId::new(7);
        model.upsert_window(window, None, 0, b"w".to_vec());
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
    fn desync_events_are_never_ignored() {
        let mut model = model();
        model.mark_synced();
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
