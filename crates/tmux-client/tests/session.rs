//! Session-level tests with a scripted transport.
//!
//! These exercise the whole pipeline — parser, queue, model, snapshot engine,
//! resynchronisation and the incremental pane capture — without a tmux server, so
//! they pin the *semantics* independently of a particular tmux release.  The real
//! server is covered by `tools/container/e2e.sh` and the `poc` scenarios.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tmux_client::session::{ControlSession, PaneOutputState, SessionEvent};
use tmux_client::transport::{Transport, TransportError};
use tmux_protocol::limits::Limits;

type Inject = Arc<Mutex<VecDeque<Vec<u8>>>>;
type Written = Arc<Mutex<Vec<u8>>>;

fn push(inject: &Inject, bytes: &[u8]) {
    inject
        .lock()
        .expect("fake transport lock")
        .push_back(bytes.to_vec());
}

fn written_text(written: &Written) -> String {
    String::from_utf8_lossy(&written.lock().expect("written lock")).into_owned()
}

/// A fake tmux server: answers the snapshot commands, and lets a test inject
/// notifications at chosen points in the stream.
#[derive(Default)]
struct FakeTmux {
    incoming: Inject,
    written: Written,
    counter: u64,
    closed: bool,
    /// Emitted immediately before the `capture-pane` response of a pane.
    before_capture: Option<Vec<u8>>,
    /// Emitted immediately after the `capture-pane` response of a pane.
    after_capture: Option<Vec<u8>>,
    /// Set when the next read should report "nothing available".
    starve_once: bool,
    /// Body of a capture response; the default is one restored line.
    capture_lines: Vec<String>,
}

impl FakeTmux {
    fn block_of(&mut self, lines: impl IntoIterator<Item = String>) -> Vec<u8> {
        self.counter += 1;
        let number = self.counter;
        let mut out = format!("%begin 100 {number} 0\n").into_bytes();
        for line in lines {
            out.extend_from_slice(line.as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(format!("%end 100 {number} 0\n").as_bytes());
        out
    }

    fn block(&mut self, lines: &[&str]) -> Vec<u8> {
        self.block_of(lines.iter().map(|line| (*line).to_string()))
    }

    fn send(&self, bytes: Vec<u8>) {
        self.incoming
            .lock()
            .expect("fake transport lock")
            .push_back(bytes);
    }

    fn answer(&mut self, line: &str) {
        if let Some(nonce) = line.strip_prefix("display-message -p ") {
            if nonce.starts_with("__muxterm_sync__") {
                // Echo the whole marker, exactly as tmux echoes the format argument.
                let block = self.block(&[nonce]);
                self.send(block);
                return;
            }
        }
        if line.starts_with("display-message -p -t") {
            let block = self.block(&["3 2 0 0 2000 0  0"]);
            self.send(block);
            return;
        }
        if line.starts_with("list-sessions") {
            let block = self.block(&["$0 1 dev"]);
            self.send(block);
        } else if line.starts_with("list-windows") {
            let block = self.block(&["$0 @0 0 1 0 bb62,80x24,0,0,0 bb62,80x24,0,0,0 shell"]);
            self.send(block);
        } else if line.starts_with("list-panes") {
            let block = self.block(&["@0 %0 0 80 24 1 0 0 sh"]);
            self.send(block);
        } else if line.starts_with("capture-pane") {
            if let Some(before) = self.before_capture.take() {
                self.send(before);
            }
            let lines = if self.capture_lines.is_empty() {
                vec!["$ restored line".to_string()]
            } else {
                self.capture_lines.clone()
            };
            let block = self.block_of(lines);
            self.send(block);
            if let Some(after) = self.after_capture.take() {
                self.send(after);
            }
        } else {
            // Every command is answered, so the queue never holds an in-flight
            // slot for a command the fake silently ignored.
            let block = self.block(&[]);
            self.send(block);
        }
    }
}

impl Transport for FakeTmux {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        self.written
            .lock()
            .expect("written lock")
            .extend_from_slice(bytes);
        let text = String::from_utf8_lossy(bytes).into_owned();
        for line in text.lines() {
            if !line.is_empty() {
                self.answer(line);
            }
        }
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        // Hold the lock once: a second `lock()` on the same non-reentrant mutex
        // while the guard from an `if let` scrutinee is still alive deadlocks.
        let mut pending = self.incoming.lock().expect("fake lock");
        if let Some(bytes) = pending.pop_front() {
            // Deliver in small pieces to exercise the streaming parser.
            if bytes.len() > 16 {
                let rest = bytes[16..].to_vec();
                pending.push_front(rest);
                drop(pending);
                return Ok(Some(bytes[..16].to_vec()));
            }
            drop(pending);
            return Ok(Some(bytes));
        }
        drop(pending);
        if self.closed {
            return Err(TransportError::Closed);
        }
        if self.starve_once {
            self.starve_once = false;
            return Ok(None);
        }
        Ok(None)
    }

    fn shutdown(&mut self) {
        self.closed = true;
    }

    fn is_closed(&self) -> bool {
        self.closed
    }

    fn describe(&self) -> String {
        "fake-tmux".to_string()
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }
}

fn session_with(fake: FakeTmux) -> ControlSession {
    ControlSession::new(Box::new(fake), Limits::interactive())
}

/// Pumps until a snapshot completes, or the deadline expires.
fn sync_session(session: &mut ControlSession) -> Vec<SessionEvent> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let events = session.pump(Duration::from_millis(10)).expect("pump");
        let done = events
            .iter()
            .any(|event| matches!(event, SessionEvent::SnapshotComplete(_)));
        let exited = events
            .iter()
            .any(|event| matches!(event, SessionEvent::Exited { .. }));
        all.extend(events);
        if done || exited {
            break;
        }
    }
    all
}

/// Pumps until `predicate` sees an event or the deadline expires.
fn pump_until(
    session: &mut ControlSession,
    predicate: impl Fn(&SessionEvent) -> bool,
) -> Vec<SessionEvent> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let events = session.pump(Duration::from_millis(10)).expect("pump");
        let found = events.iter().any(&predicate);
        all.extend(events);
        if found {
            break;
        }
    }
    all
}

fn has_snapshot(events: &[SessionEvent], pane: u64) -> bool {
    let pane = tmux_protocol::ids::PaneId::new(u32::try_from(pane).expect("pane id"));
    events
        .iter()
        .any(|event| matches!(event, SessionEvent::PaneSnapshot { pane: p, .. } if *p == pane))
}

// ---------------------------------------------------------------------------
// Initial snapshot
// ---------------------------------------------------------------------------

#[test]
fn snapshot_completes_and_marks_the_model_trustworthy() {
    let mut session = session_with(FakeTmux::default());
    let events = sync_session(&mut session);

    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::SnapshotComplete(_))),
        "snapshot must complete: {events:?}"
    );
    assert!(session.model().consistency().is_synced());
    assert_eq!(session.model().sessions().len(), 1);
    assert_eq!(session.model().windows().len(), 1);
    assert_eq!(session.model().panes().len(), 1);
    let pane = tmux_protocol::ids::PaneId::new(0);
    assert!(session.model().pane_is_ready(pane));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::PaneSnapshot { pane: p, .. } if *p == pane
    )));
    // The session→window relation is filled from list-windows, not left empty.
    let session_id = tmux_protocol::ids::SessionId::new(0);
    assert_eq!(
        session.model().sessions()[&session_id].windows,
        vec![tmux_protocol::ids::WindowId::new(0)]
    );
    assert_eq!(
        session.model().sessions()[&session_id].current_window,
        Some(tmux_protocol::ids::WindowId::new(0))
    );
}

#[test]
fn output_before_the_capture_is_dropped_and_output_after_is_delivered() {
    let fake = FakeTmux {
        before_capture: Some(b"%output %0 PRE-CAPTURE-OUTPUT\n".to_vec()),
        after_capture: Some(b"%output %0 POST-CAPTURE-OUTPUT\n".to_vec()),
        ..FakeTmux::default()
    };
    let mut session = session_with(fake);
    let events = sync_session(&mut session);

    let delivered: Vec<Vec<u8>> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::PaneOutput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert!(
        delivered.iter().any(|data| data == b"POST-CAPTURE-OUTPUT"),
        "output after the watermark must be delivered: {delivered:?}"
    );
    assert!(
        !delivered.iter().any(|data| data == b"PRE-CAPTURE-OUTPUT"),
        "output before the watermark is already in the snapshot: {delivered:?}"
    );
    assert_eq!(
        session.model().dropped_output_bytes(),
        b"PRE-CAPTURE-OUTPUT".len() as u64,
        "dropping must be observable"
    );
}

#[test]
fn snapshot_happens_before_application_output_is_trusted() {
    let fake = FakeTmux {
        before_capture: Some(b"%output %0 early\n".to_vec()),
        after_capture: Some(b"%output %0 late\n".to_vec()),
        ..FakeTmux::default()
    };
    let mut session = session_with(fake);
    let events = sync_session(&mut session);

    let snapshot_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::PaneSnapshot { .. }))
        .expect("a snapshot must be delivered");
    let output_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::PaneOutput { .. }))
        .expect("live output must be delivered");
    assert!(
        snapshot_index < output_index,
        "snapshot at {snapshot_index} must precede live output at {output_index}"
    );
}

#[test]
fn exit_closes_the_queue_permanently() {
    let fake = FakeTmux {
        after_capture: Some(b"%exit\n".to_vec()),
        ..FakeTmux::default()
    };
    let mut session = session_with(fake);
    let events = sync_session(&mut session);

    assert!(session.is_closed(), "an %exit must close the session");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Exited { .. })),
        "an Exited event must be reported: {events:?}"
    );

    // After the exit the session must refuse new commands rather than write them:
    // a late command written to a terminal that has gone back to the shell is the
    // failure mode this whole rule exists to prevent.
    assert!(
        session
            .request(&tmux_protocol::command::Command::new("list-sessions"))
            .is_err(),
        "a closed session must refuse commands"
    );
    let after = session.pump(Duration::from_millis(10)).expect("pump");
    assert!(
        !after
            .iter()
            .any(|event| matches!(event, SessionEvent::PaneOutput { .. })),
        "no output can arrive after the exit: {after:?}"
    );
}

#[test]
fn pump_until_synced_errors_when_the_connection_ends_first() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    let mut session = session_with(fake);
    push(&inject, b"%exit\n");
    let result = session.pump_until_synced(Duration::from_secs(1));
    assert!(
        matches!(
            result,
            Err(tmux_client::session::SessionError::Sync(
                tmux_client::sync::SyncError::Interrupted
            ))
        ),
        "a closed connection must not look like a completed snapshot: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Desync, truncation and error recovery
// ---------------------------------------------------------------------------

#[test]
fn malformed_output_desynchronises_and_is_not_papered_over() {
    let fake = FakeTmux {
        after_capture: Some(b"%output %0 bad\\qescape\n".to_vec()),
        ..FakeTmux::default()
    };
    let mut session = session_with(fake);
    let events = sync_session(&mut session);

    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Desync { .. })),
        "a malformed %output must surface as a desync: {events:?}"
    );
    assert!(
        !session.model().consistency().is_synced(),
        "the model must not claim accuracy after a desync"
    );
}

#[test]
fn a_truncated_capture_is_rejected() {
    let fake = FakeTmux {
        capture_lines: vec!["old line one".to_string(), "old line two".to_string()],
        ..FakeTmux::default()
    };
    let limits = Limits {
        max_response_lines: 1,
        ..Limits::interactive()
    };
    let mut session = ControlSession::new(Box::new(fake), limits);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut error = None;
    while Instant::now() < deadline && error.is_none() {
        match session.pump(Duration::from_millis(10)) {
            Ok(_) => {}
            Err(err) => error = Some(err.to_string()),
        }
    }
    let error = error.expect("a truncated snapshot must fail");
    assert!(
        error.contains("Truncated") || error.contains("exceeded the configured limits"),
        "the error must name truncation, got {error}"
    );
    assert!(
        !session.model().consistency().is_synced(),
        "a truncated snapshot must never mark the model synced"
    );
}

// ---------------------------------------------------------------------------
// Resynchronisation and incremental pane capture
// ---------------------------------------------------------------------------

#[test]
fn sessions_changed_triggers_a_resync() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);
    assert!(session.model().consistency().is_synced());

    push(&inject, b"%sessions-changed\n");
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut completions = 0;
    while Instant::now() < deadline && completions == 0 {
        for event in session.pump(Duration::from_millis(10)).expect("pump") {
            if matches!(event, SessionEvent::SnapshotComplete(_)) {
                completions += 1;
            }
        }
    }
    assert_eq!(
        completions, 1,
        "a resync must complete after sessions-changed"
    );
    assert!(session.model().consistency().is_synced());
}

#[test]
fn a_pane_discovered_by_a_layout_change_is_captured_then_live() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);

    push(
        &inject,
        b"%layout-change @0 bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1] \
          bb62,80x24,0,0[80x12,0,0,0,80x11,0,13,1]\n",
    );
    let events = pump_until(&mut session, |event| {
        matches!(
            event,
            SessionEvent::PaneSnapshot { pane, .. }
                if *pane == tmux_protocol::ids::PaneId::new(1)
        )
    });
    assert!(
        has_snapshot(&events, 1),
        "a pane discovered after the snapshot must get its own capture: {events:?}"
    );
    assert!(
        session
            .model()
            .pane_is_ready(tmux_protocol::ids::PaneId::new(1)),
        "the discovered pane must become live"
    );

    push(&inject, b"%output %1 NEW-PANE\n");
    let events = pump_until(&mut session, |event| {
        matches!(
            event,
            SessionEvent::PaneOutput { pane, data }
                if *pane == tmux_protocol::ids::PaneId::new(1) && data == b"NEW-PANE"
        )
    });
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::PaneOutput { pane, data }
                if *pane == tmux_protocol::ids::PaneId::new(1) && data == b"NEW-PANE"
        )),
        "output for the new pane must be delivered after its capture: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// Flow control and client size
// ---------------------------------------------------------------------------

#[test]
fn pause_and_continue_are_surfaced_and_continue_is_encoded() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    let written = Arc::clone(&fake.written);
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);

    push(&inject, b"%pause %0\n%continue %0\n");
    let events = pump_until(&mut session, |event| {
        matches!(event, SessionEvent::OutputResumed { .. })
    });
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::OutputPaused { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::OutputResumed { .. })));

    session
        .set_pane_output(
            tmux_protocol::ids::PaneId::new(0),
            PaneOutputState::Continue,
        )
        .expect("resume is queued");
    let _ = session.pump(Duration::from_millis(50));
    assert!(
        written_text(&written).contains("refresh-client -A %0:continue"),
        "the resume command must be encoded: {}",
        written_text(&written)
    );
}

#[test]
fn client_and_window_size_are_encoded() {
    let fake = FakeTmux::default();
    let written = Arc::clone(&fake.written);
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);

    session.resize_client(120, 40).expect("client size queued");
    session
        .resize_window(tmux_protocol::ids::WindowId::new(0), 100, 30)
        .expect("window size queued");
    let _ = session.pump(Duration::from_millis(50));
    let text = written_text(&written);
    assert!(text.contains("refresh-client -C 120x40"), "written: {text}");
    assert!(
        text.contains("refresh-client -C @0:100x30"),
        "written: {text}"
    );
}

// ---------------------------------------------------------------------------
// Notifications that used to be swallowed
// ---------------------------------------------------------------------------

#[test]
fn raw_lines_messages_and_config_errors_are_surfaced() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);

    push(
        &inject,
        b"this is not protocol\n%config-error bad option\n%message hello\n%future-thing x\n",
    );
    let events = pump_until(&mut session, |event| {
        matches!(event, SessionEvent::Unknown { .. })
    });
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::ProtocolNoise { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::ConfigError { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Message { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::Unknown { .. })));
}

// ---------------------------------------------------------------------------
// -CC takeover integration
// ---------------------------------------------------------------------------

#[test]
fn takeover_routes_shell_and_control_bytes_separately() {
    let fake = FakeTmux::default();
    let inject = Arc::clone(&fake.incoming);
    push(&inject, b"\x1bP1000p");
    let mut session = ControlSession::new_with_takeover(Box::new(fake), Limits::interactive());
    let events = session
        .pump_until_synced(Duration::from_secs(2))
        .expect("takeover snapshot completes");
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::ControlModeEntered)));
    assert!(events
        .iter()
        .any(|event| matches!(event, SessionEvent::SnapshotComplete(_))));

    push(&inject, b"%exit\n\x1b\\$ ");
    let events = pump_until(&mut session, |event| {
        matches!(event, SessionEvent::Exited { .. })
    });
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::ControlModeExited)),
        "the takeover must report the switch back: {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            SessionEvent::TerminalOutput(data) if data == b"$ "
        )),
        "post-control shell bytes must go to the terminal: {events:?}"
    );
    assert!(session.is_closed());
}

// ---------------------------------------------------------------------------
// Application commands
// ---------------------------------------------------------------------------

#[test]
fn an_application_command_gets_its_own_response_with_the_server_header() {
    let mut session = session_with(FakeTmux::default());
    let _ = sync_session(&mut session);

    let id = session
        .request(&tmux_protocol::command::Command::new("list-sessions"))
        .expect("a synced session accepts commands");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = None;
    while Instant::now() < deadline && response.is_none() {
        for event in session.pump(Duration::from_millis(20)).expect("pump") {
            if let SessionEvent::Response(found) = event {
                response = Some(found);
            }
        }
    }

    let response = response.expect("the command must be answered");
    assert_eq!(response.request, Some(id));
    assert!(response.ok, "the fake answers successfully");
    assert!(
        response.header.number > 0,
        "the server's own framing numbers are preserved"
    );
    assert_eq!(response.first_line(), Some(&b"$0 1 dev"[..]));
}

#[test]
fn a_failed_snapshot_command_stops_the_snapshot() {
    struct Failing(FakeTmux);
    impl Transport for Failing {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.0.write_all(bytes)?;
            let text = String::from_utf8_lossy(bytes).into_owned();
            if text.starts_with("list-windows") {
                self.0.incoming.lock().expect("fake lock").clear();
                self.0.counter += 1;
                let number = self.0.counter;
                let block =
                    format!("%begin 100 {number} 0\nno such window\n%error 100 {number} 0\n");
                self.0.send(block.into_bytes());
            }
            Ok(())
        }
        fn recv(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
            self.0.recv(timeout)
        }
        fn shutdown(&mut self) {
            self.0.shutdown();
        }
        fn is_closed(&self) -> bool {
            self.0.is_closed()
        }
        fn describe(&self) -> String {
            "failing-fake".to_string()
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
    }

    let mut session = ControlSession::new(
        Box::new(Failing(FakeTmux::default())),
        Limits::interactive(),
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut error = None;
    while Instant::now() < deadline && error.is_none() {
        match session.pump(Duration::from_millis(10)) {
            Ok(_) => {}
            Err(err) => error = Some(err.to_string()),
        }
    }
    let error = error.expect("a failed snapshot command must surface as an error");
    assert!(
        error.contains("ListWindows") || error.contains("list-windows"),
        "the error must name the failing stage, got {error}"
    );
}
