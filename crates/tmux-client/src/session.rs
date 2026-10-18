//! The control-mode session driver: transport + parser + queue + model + snapshot.
//!
//! A session is deliberately single-threaded: bytes are read, parsed and applied in
//! one place, which is what makes the snapshot watermark rule in [`crate::sync`]
//! hold.  Anything that needs concurrency (rendering, a GUI event loop) consumes
//! [`SessionEvent`]s instead of touching the connection.
//!
//! # Lifecycle
//!
//! * **attach** starts one snapshot (barrier → sessions → windows → panes →
//!   per-pane capture/meta);
//! * **resync** re-runs the same sequence on demand, and is also started
//!   automatically after `%sessions-changed` or a protocol desync (bounded by
//!   [`MAX_AUTO_RESYNCS`], so a permanently broken stream cannot spin);
//! * a pane discovered by a layout change after the snapshot gets its own small
//!   capture/meta exchange before its output is delivered;
//! * `%pause`/`%continue` are surfaced as events and can be answered with
//!   [`ControlSession::set_pane_output`];
//! * the client size is announced with [`ControlSession::resize_client`], which is
//!   what keeps the remote window and the GUI in step.
//!
//! # `-CC`
//!
//! [`ControlSession::new_with_takeover`] routes a byte stream through a
//! [`TakeoverSwitch`] first, so a caller that owns a PTY gets the shell-before /
//! control-mode / shell-after transitions as events.  [`ProcessTransport`] uses
//! pipes and is therefore only suitable for `-C`; the switch path needs a PTY
//! transport supplied by the caller.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use tmux_protocol::command::{send_keys, Command, KeySpec};
use tmux_protocol::ids::{PaneId, WindowId};
use tmux_protocol::limits::Limits;
use tmux_protocol::parser::{Event, Parser};
use tmux_protocol::queue::{CommandQueue, QueueError, RequestId, Response};
use tmux_protocol::switch::{SwitchAction, TakeoverSwitch};
use tmux_protocol::SYNC_MARKER;

use crate::epoch::ConnectionEpoch;
use crate::model::{ModelChange, RemoteModel};
use crate::sync::{
    apply_meta, capture_command, meta_command, parse_pane_meta, SyncEngine, SyncError, SyncOutcome,
    SyncStats, Tag,
};
use crate::transport::{ProcessTransport, Target, TargetError, Transport, TransportError};

/// How many automatic resynchronisations are attempted before the session stops
/// retrying and leaves the model untrusted for the caller to repair explicitly.
pub const MAX_AUTO_RESYNCS: u32 = 3;

/// Something a consumer (UI, test, logger) must react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    /// Pane output that may be fed to that pane's terminal emulator.
    PaneOutput {
        /// Source pane.
        pane: PaneId,
        /// Raw bytes.
        data: Vec<u8>,
    },
    /// Bytes that belong to the local terminal, not to control mode.
    ///
    /// Only produced by a session created with
    /// [`ControlSession::new_with_takeover`]: it is the shell prompt before the
    /// takeover and the shell again after it.
    TerminalOutput(Vec<u8>),
    /// A pane's screen snapshot.  It must be loaded into the pane's terminal
    /// emulator *before* the next event is processed.
    PaneSnapshot {
        /// Pane the snapshot belongs to.
        pane: PaneId,
        /// Captured lines, with escape sequences when tmux emitted any.
        lines: Vec<Vec<u8>>,
    },
    /// The remote model changed.
    Model(ModelChange),
    /// A command the caller issued was answered.
    Response(Response),
    /// The server answered something the client never asked (initialisation).
    Unsolicited(Response),
    /// The initial snapshot completed; the model is trustworthy only if
    /// [`crate::RemoteModel::consistency`] says so.
    SnapshotComplete(SyncStats),
    /// The protocol desynchronised: the session must be rebuilt or resynchronised
    /// before rendering.
    Desync {
        /// Why.
        reason: String,
    },
    /// The server paused output for a pane (`%pause`).
    OutputPaused {
        /// Affected pane.
        pane: PaneId,
    },
    /// The server resumed output for a pane (`%continue`).
    OutputResumed {
        /// Affected pane.
        pane: PaneId,
    },
    /// The `-CC` DCS takeover started.
    ControlModeEntered,
    /// The `-CC` takeover ended and the shell owns the terminal again.
    ControlModeExited,
    /// A line outside a response block did not start with `%`.
    ///
    /// In control mode this means the stream is not (or no longer) protocol; it
    /// is the signal that would expose a shell leak, so it is never swallowed.
    ProtocolNoise {
        /// The offending line.
        line: Vec<u8>,
    },
    /// A `%message` forwarded by tmux.
    Message {
        /// Message text.
        text: Vec<u8>,
    },
    /// A `%config-error` reported by tmux.
    ConfigError {
        /// Error text.
        text: Vec<u8>,
    },
    /// A notification this implementation does not know.
    Unknown {
        /// The `%verb` token.
        verb: Vec<u8>,
        /// Everything after the verb.
        rest: Vec<u8>,
    },
    /// The session ended.
    Exited {
        /// Reason reported by the server, if any.
        reason: Option<String>,
    },
}

/// State a control client can request for a pane's output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneOutputState {
    /// Deliver output normally.
    On,
    /// Stop delivering output for this pane.
    Off,
    /// Pause the pane.
    Pause,
    /// Resume a paused pane.
    Continue,
}

impl PaneOutputState {
    /// The token `refresh-client -A` expects.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Pause => "pause",
            Self::Continue => "continue",
        }
    }
}

/// Failures of the session driver.
#[derive(Debug)]
pub enum SessionError {
    /// A target cannot be spawned safely.
    Target(TargetError),
    /// The client process could not be started.
    Spawn(std::io::Error),
    /// The transport failed.
    Transport(TransportError),
    /// A command could not be scheduled.
    Queue(QueueError),
    /// The snapshot failed.
    Sync(SyncError),
    /// The session is closed.
    Closed,
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Target(err) => write!(f, "{err}"),
            Self::Spawn(err) => write!(f, "could not start the tmux client: {err}"),
            Self::Transport(err) => write!(f, "{err}"),
            Self::Queue(err) => write!(f, "{err}"),
            Self::Sync(err) => write!(f, "{err}"),
            Self::Closed => write!(f, "session is closed"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<TargetError> for SessionError {
    fn from(err: TargetError) -> Self {
        Self::Target(err)
    }
}

impl From<TransportError> for SessionError {
    fn from(err: TransportError) -> Self {
        Self::Transport(err)
    }
}

impl From<QueueError> for SessionError {
    fn from(err: QueueError) -> Self {
        Self::Queue(err)
    }
}

impl From<SyncError> for SessionError {
    fn from(err: SyncError) -> Self {
        Self::Sync(err)
    }
}

/// Which kind of request a [`RequestId`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestTag {
    /// A command of the ordered snapshot sequence.
    Sync(Tag),
    /// `capture-pane` for a newly discovered pane.
    PaneRefreshCapture,
    /// `display-message` metadata for a newly discovered pane.
    PaneRefreshMeta,
}

/// Stage of the small capture/meta exchange used for a pane discovered after the
/// initial snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneRefreshStage {
    Capture,
    Meta,
}

#[derive(Debug)]
struct PaneRefresh {
    pane: PaneId,
    stage: PaneRefreshStage,
    lines: Vec<Vec<u8>>,
}

/// One control-mode connection to one tmux server.
pub struct ControlSession {
    epoch: ConnectionEpoch,
    transport: Box<dyn Transport>,
    parser: Parser,
    queue: CommandQueue,
    model: RemoteModel,
    sync: Option<SyncEngine>,
    /// `true` when the next `advance_snapshot` may schedule a command.
    tags: HashMap<RequestId, (RequestTag, Option<PaneId>)>,
    limits: Limits,
    closed: bool,
    exit_reason: Option<String>,
    /// The barrier marker of the current snapshot.
    marker: String,
    /// Incremented on every resynchronisation so markers never repeat.
    sync_round: u64,
    /// Set by a `-CC` session; `None` for a plain `-C` pipe.
    switch: Option<TakeoverSwitch>,
    /// A resync is wanted but could not start yet (queue busy).
    needs_resync: bool,
    /// Automatic resync attempts for the current failure streak.
    auto_resyncs: u32,
    /// Panes discovered by a layout change, waiting for capture.
    pane_refresh: VecDeque<PaneId>,
    /// The capture/meta exchange currently in flight.
    refresh: Option<PaneRefresh>,
}

impl ControlSession {
    /// Spawns the client described by `target` and starts the snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Target`] when the target cannot be spawned safely,
    /// and [`SessionError::Spawn`] when the process cannot be started.
    pub fn attach(target: &Target, limits: Limits) -> Result<Self, SessionError> {
        target.validate()?;
        let transport = ProcessTransport::spawn(target).map_err(SessionError::Spawn)?;
        Ok(Self::build(Box::new(transport), limits, None))
    }

    /// Wraps an already connected transport.
    #[must_use]
    pub fn new(transport: Box<dyn Transport>, limits: Limits) -> Self {
        Self::build(transport, limits, None)
    }

    /// Wraps a transport whose bytes carry a `-CC` takeover stream (a PTY).
    ///
    /// The caller must use a PTY-backed transport; [`ProcessTransport`] uses pipes
    /// and should be used with [`ControlSession::attach`] (`-C`) instead.
    #[must_use]
    pub fn new_with_takeover(transport: Box<dyn Transport>, limits: Limits) -> Self {
        Self::build(transport, limits, Some(TakeoverSwitch::new(limits)))
    }

    fn build(
        transport: Box<dyn Transport>,
        limits: Limits,
        switch: Option<TakeoverSwitch>,
    ) -> Self {
        debug_assert!(
            limits.validate().is_ok(),
            "zero limits would silently stall the session"
        );
        let epoch = ConnectionEpoch::next();
        let marker = format!("{SYNC_MARKER}{}-0", epoch.as_u64());
        let mut model = RemoteModel::new(epoch, limits);
        model.begin_snapshot();
        Self {
            epoch,
            transport,
            parser: Parser::new(limits),
            queue: CommandQueue::new(limits),
            model,
            sync: Some(SyncEngine::new(marker.clone(), limits)),
            tags: HashMap::new(),
            limits,
            closed: false,
            exit_reason: None,
            marker,
            sync_round: 0,
            switch,
            needs_resync: false,
            auto_resyncs: 0,
            pane_refresh: VecDeque::new(),
            refresh: None,
        }
    }

    /// The connection epoch of this session.
    #[must_use]
    pub const fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// The remote model.
    #[must_use]
    pub const fn model(&self) -> &RemoteModel {
        &self.model
    }

    /// Mutable access to the model (used by tests and by the UI layer).
    #[must_use]
    pub const fn model_mut(&mut self) -> &mut RemoteModel {
        &mut self.model
    }

    /// True once the connection is unusable.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// The reason the session ended, if it did.
    #[must_use]
    pub fn exit_reason(&self) -> Option<&str> {
        self.exit_reason.as_deref()
    }

    /// A description of the underlying transport.
    #[must_use]
    pub fn describe(&self) -> String {
        self.transport.describe()
    }

    /// Recent stderr from the client process.
    #[must_use]
    pub fn stderr_tail(&self) -> String {
        self.transport.stderr_tail()
    }

    /// True while the ordered snapshot sequence is still running.
    #[must_use]
    fn sync_active(&self) -> bool {
        self.sync
            .as_ref()
            .is_some_and(|engine| !engine.is_finished())
    }

    /// Schedules a command that is not part of the snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Closed`] when the session ended.
    pub fn request(&mut self, command: &Command) -> Result<RequestId, SessionError> {
        if self.closed {
            return Err(SessionError::Closed);
        }
        Ok(self.queue.enqueue(command)?)
    }

    /// Sends typed key input to a pane.
    ///
    /// # Errors
    ///
    /// Propagates encoding and queue failures.
    pub fn send_keys(
        &mut self,
        pane: PaneId,
        specs: &[KeySpec],
    ) -> Result<Vec<RequestId>, SessionError> {
        let encoded = send_keys(pane, specs, self.limits).map_err(QueueError::Encoding)?;
        let mut ids = Vec::with_capacity(encoded.len());
        for bytes in encoded {
            ids.push(self.queue.enqueue_encoded(bytes)?);
        }
        Ok(ids)
    }

    /// Sends raw bytes to a pane as if the terminal had produced them.
    ///
    /// This is the path for mouse reports, bracketed paste and focus events
    /// produced by `term_core::input`: every byte value survives because the
    /// command is `send-keys -H`.
    ///
    /// # Errors
    ///
    /// Propagates encoding and queue failures.
    pub fn send_bytes(
        &mut self,
        pane: PaneId,
        bytes: Vec<u8>,
    ) -> Result<Vec<RequestId>, SessionError> {
        self.send_keys(pane, &[KeySpec::Bytes(bytes)])
    }

    /// Asks tmux to change how a pane's output is delivered to this client.
    ///
    /// `%pause` means the server has stopped sending output; the GUI should
    /// drain what it has and then call this with [`PaneOutputState::Continue`].
    ///
    /// # Errors
    ///
    /// Propagates queue failures and [`SessionError::Closed`].
    pub fn set_pane_output(
        &mut self,
        pane: PaneId,
        state: PaneOutputState,
    ) -> Result<RequestId, SessionError> {
        let argument = format!("{pane}:{}", state.as_str());
        self.request(&Command::new("refresh-client").arg("-A").arg(argument))
    }

    /// Announces this control client's size to tmux.
    ///
    /// Without this, tmux keeps sizing the window from whatever default it gave
    /// the control client, and the GUI cannot drive pane geometry.
    ///
    /// # Errors
    ///
    /// Propagates queue failures and [`SessionError::Closed`].
    pub fn resize_client(&mut self, columns: u16, rows: u16) -> Result<RequestId, SessionError> {
        self.request(&Command::new("refresh-client").flag("-C", format!("{columns}x{rows}")))
    }

    /// Announces the size of one window to tmux.
    ///
    /// Useful when a window is larger than the client (control-mode clients may
    /// size windows independently).
    ///
    /// # Errors
    ///
    /// Propagates queue failures and [`SessionError::Closed`].
    pub fn resize_window(
        &mut self,
        window: WindowId,
        columns: u16,
        rows: u16,
    ) -> Result<RequestId, SessionError> {
        self.request(
            &Command::new("refresh-client").flag("-C", format!("{window}:{columns}x{rows}")),
        )
    }

    /// Starts a full resynchronisation.
    ///
    /// The snapshot is scheduled immediately when the queue is idle; otherwise it
    /// starts on a later [`ControlSession::pump`].  The model is marked
    /// untrustworthy until the new snapshot completes.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Closed`] when the session ended.
    pub fn resync(&mut self) -> Result<(), SessionError> {
        if self.closed {
            return Err(SessionError::Closed);
        }
        self.needs_resync = true;
        self.auto_resyncs = 0;
        self.try_start_resync()
    }

    /// Reads and processes everything available within `timeout`.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Sync`] when the snapshot fails and
    /// [`SessionError::Transport`] for unrecoverable transport failures.
    pub fn pump(&mut self, timeout: Duration) -> Result<Vec<SessionEvent>, SessionError> {
        let mut events = Vec::new();
        self.flush_writes(&mut events)?;

        if !self.closed {
            match self.transport.recv(timeout) {
                Ok(Some(bytes)) => self.consume(&bytes, &mut events)?,
                Ok(None) | Err(TransportError::Timeout) => {}
                Err(TransportError::Closed) => self.close_with(None, &mut events),
                Err(err) => self.close_with(Some(err.to_string()), &mut events),
            }
        }

        if !self.closed {
            self.advance_snapshot()?;
            self.advance_refresh()?;
            self.try_start_resync()?;
            self.flush_writes(&mut events)?;
        }
        Ok(events)
    }

    /// Pumps until the snapshot completes or `timeout` elapses.
    ///
    /// # Errors
    ///
    /// Propagates [`ControlSession::pump`] failures, and returns
    /// [`SyncError::Interrupted`] when the connection ends before a snapshot
    /// completes (a completed `Exited` must not look like success).
    pub fn pump_until_synced(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let deadline = Instant::now() + timeout;
        let mut collected = Vec::new();
        loop {
            if self.closed {
                return Err(SessionError::Sync(SyncError::Interrupted));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Sync(SyncError::Interrupted));
            }
            let events = self.pump(remaining.min(Duration::from_millis(100)))?;
            let done = events
                .iter()
                .any(|event| matches!(event, SessionEvent::SnapshotComplete(_)));
            collected.extend(events);
            if done {
                return Ok(collected);
            }
            if self.closed {
                return Err(SessionError::Sync(SyncError::Interrupted));
            }
        }
    }

    /// Closes the session, detaching from the tmux server without killing it.
    pub fn close(&mut self) {
        let mut ignored = Vec::new();
        self.close_with(Some("closed by the client".to_string()), &mut ignored);
    }

    fn close_with(&mut self, reason: Option<String>, events: &mut Vec<SessionEvent>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.exit_reason.clone_from(&reason);
        self.transport.shutdown();
        self.refresh = None;
        self.pane_refresh.clear();
        self.needs_resync = false;
        if let Some(switch) = self.switch.as_mut() {
            let mut actions = Vec::new();
            switch.force_terminal(&mut actions);
            if actions
                .iter()
                .any(|action| matches!(action, SwitchAction::ExitControlMode))
            {
                events.push(SessionEvent::ControlModeExited);
            }
        }
        for response in self.queue.close() {
            events.push(SessionEvent::Response(response));
        }
        events.push(SessionEvent::Exited { reason });
    }

    /// Feeds one chunk of transport bytes through the switch (when present) and
    /// the parser, then handles every event in order.
    fn consume(
        &mut self,
        bytes: &[u8],
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        let mut parsed = Vec::new();
        if self.switch.is_some() {
            let mut actions = Vec::new();
            if let Some(switch) = self.switch.as_mut() {
                switch.feed(bytes, &mut actions);
            }
            for action in actions {
                match action {
                    SwitchAction::Terminal(data) => {
                        events.push(SessionEvent::TerminalOutput(data));
                    }
                    SwitchAction::Control(data) => self.parser.feed(&data, &mut parsed),
                    SwitchAction::EnterControlMode => {
                        events.push(SessionEvent::ControlModeEntered);
                    }
                    SwitchAction::ExitControlMode => {
                        events.push(SessionEvent::ControlModeExited);
                    }
                }
            }
        } else {
            self.parser.feed(bytes, &mut parsed);
        }

        for event in parsed {
            if self.closed {
                break;
            }
            self.handle_event(event, events)?;
        }
        Ok(())
    }

    fn flush_writes(&mut self, events: &mut Vec<SessionEvent>) -> Result<(), SessionError> {
        while let Some((request, bytes)) = self.queue.next_to_write() {
            if let Err(err) = self.transport.write_all(&bytes) {
                // The command never reached the server.  Only queries are retried
                // by the caller; nothing is replayed automatically (a replayed
                // keystroke or a re-run `kill-window` is worse than a lost one).
                let reason = format!("write failed: {err} ({request})");
                self.close_with(Some(reason), events);
                return Ok(());
            }
        }
        Ok(())
    }

    fn advance_snapshot(&mut self) -> Result<(), SessionError> {
        let next = match self.sync.as_mut() {
            Some(engine) if !engine.is_finished() => engine.next_command(),
            _ => None,
        };
        if let Some(tagged) = next {
            let barrier_marker =
                (tagged.tag == Tag::Barrier).then(|| self.marker.as_bytes().to_vec());
            let id = self.queue.enqueue(&tagged.command)?;
            if let Some(marker) = barrier_marker {
                self.queue.set_barrier(id, marker)?;
            }
            self.tags
                .insert(id, (RequestTag::Sync(tagged.tag), tagged.pane));
        }
        Ok(())
    }

    /// Starts the capture/meta exchange for one discovered pane, when the queue
    /// is free and the full snapshot is not running.
    fn advance_refresh(&mut self) -> Result<(), SessionError> {
        if self.closed
            || self.refresh.is_some()
            || self.sync_active()
            || self.queue.outstanding() > 0
            || self.queue.has_open_response()
        {
            return Ok(());
        }
        while let Some(pane) = self.pane_refresh.pop_front() {
            if self.model.pane_is_ready(pane) {
                continue;
            }
            let height = self
                .model
                .panes()
                .get(&pane)
                .map_or(0, |state| state.height);
            let history = self.limits.max_response_lines.saturating_sub(height);
            let id = self.queue.enqueue(&capture_command(pane, history))?;
            self.tags
                .insert(id, (RequestTag::PaneRefreshCapture, Some(pane)));
            self.refresh = Some(PaneRefresh {
                pane,
                stage: PaneRefreshStage::Capture,
                lines: Vec::new(),
            });
            return Ok(());
        }
        Ok(())
    }

    fn try_start_resync(&mut self) -> Result<(), SessionError> {
        if self.closed
            || !self.needs_resync
            || self.sync_active()
            || self.queue.outstanding() > 0
            || self.queue.has_open_response()
        {
            return Ok(());
        }
        if self.auto_resyncs >= MAX_AUTO_RESYNCS {
            return Ok(());
        }

        self.queue.clear_barrier();
        self.tags.clear();
        self.refresh = None;
        self.pane_refresh.clear();
        self.sync_round = self.sync_round.saturating_add(1);
        self.marker = format!("{SYNC_MARKER}{}-{}", self.epoch.as_u64(), self.sync_round);
        self.model.begin_snapshot();
        self.sync = Some(SyncEngine::new(self.marker.clone(), self.limits));
        self.needs_resync = false;
        self.auto_resyncs = self.auto_resyncs.saturating_add(1);
        self.advance_snapshot()
    }

    fn handle_event(
        &mut self,
        event: Event,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        // Responses are routed through the queue first: it owns the framing.
        if let Some(response) = self.queue.on_event(&event) {
            self.handle_response(response, events)?;
        }

        match event {
            Event::Output { pane, data } | Event::ExtendedOutput { pane, data, .. } => {
                if self.model.pane_is_ready(pane) {
                    events.push(SessionEvent::PaneOutput { pane, data });
                } else {
                    // Already contained in the pending snapshot: dropping it is
                    // correct, but it must be observable.
                    self.model.note_dropped_output(data.len());
                }
            }
            Event::Pause { pane } => events.push(SessionEvent::OutputPaused { pane }),
            Event::Continue { pane } => events.push(SessionEvent::OutputResumed { pane }),
            Event::Exit { reason } => {
                if let Some(switch) = self.switch.as_mut() {
                    switch.note_protocol_exit();
                }
                let text = reason.map(|r| String::from_utf8_lossy(&r).into_owned());
                self.close_with(text, events);
            }
            Event::Desync { reason, .. } => {
                let text = reason.as_str().to_string();
                let change = self
                    .model
                    .mark_desynchronised(format!("protocol desync: {text}"));
                events.push(SessionEvent::Model(change));
                events.push(SessionEvent::Desync { reason: text });
                self.needs_resync = true;
            }
            Event::SessionsChanged => {
                let changes = self.model.apply(&event);
                events.extend(changes.into_iter().map(SessionEvent::Model));
                self.needs_resync = true;
            }
            Event::Raw { line } => events.push(SessionEvent::ProtocolNoise { line }),
            Event::Message { text } => events.push(SessionEvent::Message { text }),
            Event::ConfigError { text } => events.push(SessionEvent::ConfigError { text }),
            Event::Unknown { verb, rest } => {
                events.push(SessionEvent::Unknown { verb, rest });
            }
            other => {
                for change in self.model.apply(&other) {
                    match change {
                        ModelChange::PaneDiscovered(pane) => self.schedule_pane_refresh(pane),
                        change => events.push(SessionEvent::Model(change)),
                    }
                }
            }
        }
        Ok(())
    }

    fn schedule_pane_refresh(&mut self, pane: PaneId) {
        if self.model.pane_is_ready(pane) {
            return;
        }
        if !self.pane_refresh.contains(&pane) {
            self.pane_refresh.push_back(pane);
        }
    }

    fn handle_response(
        &mut self,
        response: Response,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        let Some(request) = response.request else {
            events.push(SessionEvent::Unsolicited(response));
            return Ok(());
        };
        let Some((tag, pane)) = self.tags.remove(&request) else {
            events.push(SessionEvent::Response(response));
            return Ok(());
        };

        match tag {
            RequestTag::Sync(tag) => self.handle_sync_response(tag, pane, response, events),
            RequestTag::PaneRefreshCapture => self.handle_refresh_capture(pane, response, events),
            RequestTag::PaneRefreshMeta => self.handle_refresh_meta(pane, response, events),
        }
    }

    fn handle_sync_response(
        &mut self,
        tag: Tag,
        pane: Option<PaneId>,
        response: Response,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        let Some(engine) = self.sync.as_mut() else {
            events.push(SessionEvent::Unsolicited(response));
            return Ok(());
        };
        let outcome = match engine.on_response(tag, pane, &response, &mut self.model) {
            Ok(outcome) => outcome,
            Err(err) => {
                // A failed snapshot must not leave a barrier that blocks every
                // later command, and it must not spin retrying a broken stream.
                self.queue.clear_barrier();
                let change = self
                    .model
                    .mark_desynchronised(format!("snapshot failed: {err}"));
                events.push(SessionEvent::Model(change));
                events.push(SessionEvent::Desync {
                    reason: err.to_string(),
                });
                self.needs_resync = false;
                return Err(SessionError::Sync(err));
            }
        };

        // Events that must be delivered after the model changes they accompany.
        let mut deferred = Vec::new();
        let changes = match outcome {
            SyncOutcome::Progress { changes } => changes,
            SyncOutcome::PaneCaptured {
                pane,
                lines,
                changes,
            } => {
                deferred.push(SessionEvent::PaneSnapshot { pane, lines });
                changes
            }
            SyncOutcome::Done { stats, changes } => {
                for change in changes {
                    events.push(SessionEvent::Model(change));
                }
                if let Some(change) = self.model.mark_synced() {
                    events.extend(
                        self.model
                            .prune_unseen()
                            .into_iter()
                            .map(SessionEvent::Model),
                    );
                    events.push(SessionEvent::Model(change));
                    self.auto_resyncs = 0;
                } else {
                    // The sequence finished, but something desynchronised while it
                    // ran; the model must stay untrusted and a retry is wanted.
                    events.push(SessionEvent::Desync {
                        reason: "snapshot completed after a desync".to_string(),
                    });
                    self.needs_resync = true;
                }
                events.push(SessionEvent::SnapshotComplete(stats));
                self.tags
                    .retain(|_, (kind, _)| !matches!(kind, RequestTag::Sync(_)));
                return Ok(());
            }
        };
        for change in changes {
            events.push(SessionEvent::Model(change));
        }
        events.extend(deferred);
        Ok(())
    }

    fn handle_refresh_capture(
        &mut self,
        pane: Option<PaneId>,
        response: Response,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        let Some(pane) = pane else {
            self.refresh = None;
            return Ok(());
        };
        if response.truncated || !response.ok {
            let reason = if response.truncated {
                "pane refresh capture was truncated".to_string()
            } else {
                format!(
                    "pane refresh capture failed: {}",
                    response
                        .first_line_lossy()
                        .unwrap_or_else(|| "<no output>".to_string())
                )
            };
            self.refresh = None;
            let change = self.model.mark_desynchronised(reason.clone());
            events.push(SessionEvent::Model(change));
            events.push(SessionEvent::Desync { reason });
            return Ok(());
        }

        let Some(mut refresh) = self.refresh.take() else {
            return Ok(());
        };
        if refresh.pane != pane || refresh.stage != PaneRefreshStage::Capture {
            self.refresh = Some(refresh);
            return Ok(());
        }
        refresh.lines = response.lines;
        refresh.stage = PaneRefreshStage::Meta;
        let id = self.queue.enqueue(&meta_command(pane))?;
        self.tags
            .insert(id, (RequestTag::PaneRefreshMeta, Some(pane)));
        self.refresh = Some(refresh);
        Ok(())
    }

    fn handle_refresh_meta(
        &mut self,
        pane: Option<PaneId>,
        response: Response,
        events: &mut Vec<SessionEvent>,
    ) -> Result<(), SessionError> {
        let Some(pane) = pane else {
            self.refresh = None;
            return Ok(());
        };
        let Some(refresh) = self.refresh.take() else {
            return Ok(());
        };
        if refresh.pane != pane || refresh.stage != PaneRefreshStage::Meta {
            self.refresh = Some(refresh);
            return Ok(());
        }
        let first = response.first_line().unwrap_or_default();
        let Some(meta) = parse_pane_meta(first) else {
            let reason = format!(
                "pane {} metadata malformed: {:?}",
                pane,
                String::from_utf8_lossy(first)
            );
            let change = self.model.mark_desynchronised(reason.clone());
            events.push(SessionEvent::Model(change));
            events.push(SessionEvent::Desync { reason });
            return Ok(());
        };
        if let Some(state) = self.model.panes().get(&pane).cloned() {
            let change = self.model.upsert_pane(pane, apply_meta(&state, &meta));
            events.push(SessionEvent::Model(change));
        }
        let change = self.model.mark_pane_ready(pane);
        events.push(SessionEvent::Model(change));
        events.push(SessionEvent::PaneSnapshot {
            pane,
            lines: refresh.lines,
        });
        Ok(())
    }
}

impl Drop for ControlSession {
    fn drop(&mut self) {
        if !self.closed {
            self.transport.shutdown();
        }
    }
}
