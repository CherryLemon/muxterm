//! The control-mode session driver: transport + parser + queue + model + snapshot.
//!
//! A session is deliberately single-threaded: bytes are read, parsed and applied in
//! one place, which is what makes the snapshot watermark rule in [`crate::sync`]
//! hold.  Anything that needs concurrency (rendering, a GUI event loop) consumes
//! [`SessionEvent`]s instead of touching the connection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tmux_protocol::command::{send_keys, Command, KeySpec};
use tmux_protocol::ids::PaneId;
use tmux_protocol::limits::Limits;
use tmux_protocol::parser::{Event, Parser};
use tmux_protocol::queue::{CommandQueue, QueueError, RequestId, Response};
use tmux_protocol::SYNC_MARKER;

use crate::epoch::ConnectionEpoch;
use crate::model::{ModelChange, RemoteModel};
use crate::sync::{SyncEngine, SyncError, SyncOutcome, SyncStats, Tag};
use crate::transport::{ProcessTransport, Target, Transport, TransportError};

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
    /// The initial snapshot completed; the model is now trustworthy.
    SnapshotComplete(SyncStats),
    /// The protocol desynchronised: the session must be rebuilt before rendering.
    Desync {
        /// Why.
        reason: String,
    },
    /// The session ended.
    Exited {
        /// Reason reported by the server, if any.
        reason: Option<String>,
    },
}

/// Failures of the session driver.
#[derive(Debug)]
pub enum SessionError {
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
            Self::Spawn(err) => write!(f, "could not start the tmux client: {err}"),
            Self::Transport(err) => write!(f, "{err}"),
            Self::Queue(err) => write!(f, "{err}"),
            Self::Sync(err) => write!(f, "{err}"),
            Self::Closed => write!(f, "session is closed"),
        }
    }
}

impl std::error::Error for SessionError {}

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

/// One control-mode connection to one tmux server.
pub struct ControlSession {
    epoch: ConnectionEpoch,
    transport: Box<dyn Transport>,
    parser: Parser,
    queue: CommandQueue,
    model: RemoteModel,
    sync: Option<SyncEngine>,
    tags: HashMap<RequestId, (Tag, Option<PaneId>)>,
    limits: Limits,
    closed: bool,
    exit_reason: Option<String>,
}

impl ControlSession {
    /// Spawns the client described by `target` and starts the snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::Spawn`] when the process cannot be started.
    pub fn attach(target: &Target, limits: Limits) -> Result<Self, SessionError> {
        let transport = ProcessTransport::spawn(target).map_err(SessionError::Spawn)?;
        Ok(Self::new(Box::new(transport), limits))
    }

    /// Wraps an already connected transport.
    #[must_use]
    pub fn new(transport: Box<dyn Transport>, limits: Limits) -> Self {
        let epoch = ConnectionEpoch::next();
        let marker = format!("{SYNC_MARKER}{}", epoch.as_u64());
        Self {
            epoch,
            transport,
            parser: Parser::new(limits),
            queue: CommandQueue::new(limits),
            model: RemoteModel::new(epoch, limits),
            sync: Some(SyncEngine::new(marker)),
            tags: HashMap::new(),
            limits,
            closed: false,
            exit_reason: None,
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
                Ok(Some(bytes)) => {
                    let mut parsed = Vec::new();
                    self.parser.feed(&bytes, &mut parsed);
                    for event in parsed {
                        self.handle_event(event, &mut events)?;
                    }
                }
                Ok(None) => {}
                Err(TransportError::Closed) => {
                    self.close_with(None, &mut events);
                }
                Err(TransportError::Timeout) => {}
                Err(err) => {
                    self.close_with(Some(err.to_string()), &mut events);
                }
            }
        }

        if !self.closed {
            self.advance_snapshot()?;
            self.flush_writes(&mut events)?;
        }
        Ok(events)
    }

    /// Pumps until the snapshot completes or `timeout` elapses.
    ///
    /// # Errors
    ///
    /// Propagates [`ControlSession::pump`] failures.
    pub fn pump_until_synced(
        &mut self,
        timeout: Duration,
    ) -> Result<Vec<SessionEvent>, SessionError> {
        let deadline = Instant::now() + timeout;
        let mut collected = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Sync(SyncError::Interrupted));
            }
            let events = self.pump(remaining.min(Duration::from_millis(100)))?;
            let done = events
                .iter()
                .any(|event| matches!(event, SessionEvent::SnapshotComplete(_)));
            collected.extend(events);
            if done || self.closed {
                return Ok(collected);
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
        for response in self.queue.close() {
            events.push(SessionEvent::Response(response));
        }
        events.push(SessionEvent::Exited { reason });
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
            // The barrier command is content-verified: until its marker comes back,
            // every response block the server produces for its own initialisation
            // commands is classified as unsolicited instead of being matched to us.
            let marker = match (&self.sync, tagged.tag) {
                (Some(engine), Tag::Barrier) => Some(engine.marker().as_bytes().to_vec()),
                _ => None,
            };
            let id = self.queue.enqueue(&tagged.command)?;
            if let Some(marker) = marker {
                self.queue.set_barrier(id, marker)?;
            }
            self.tags.insert(id, (tagged.tag, tagged.pane));
        }
        Ok(())
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
            Event::Exit { reason } => {
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
            }
            Event::ResponseBegin(_)
            | Event::ResponseLine(_)
            | Event::ResponseEnd(_)
            | Event::ResponseError(_) => {}
            other => {
                for change in self.model.apply(&other) {
                    events.push(SessionEvent::Model(change));
                }
            }
        }
        Ok(())
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

        let outcome = match self.sync.as_mut() {
            Some(engine) => engine.on_response(tag, pane, &response, &mut self.model)?,
            None => SyncOutcome::Progress {
                changes: Vec::new(),
            },
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
                let mut changes = changes;
                changes.push(self.model.mark_synced());
                deferred.push(SessionEvent::SnapshotComplete(stats));
                changes
            }
        };
        for change in changes {
            events.push(SessionEvent::Model(change));
        }
        events.extend(deferred);
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
