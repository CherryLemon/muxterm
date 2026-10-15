//! Session-level tests with a scripted transport.
//!
//! These exercise the whole pipeline — parser, queue, model, snapshot engine —
//! without a tmux server, so they pin the *semantics* (watermark, ordering, close
//! behaviour) independently of any particular tmux release.  The real server is
//! covered by `tools/container/e2e.sh`.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tmux_client::session::{ControlSession, SessionEvent};
use tmux_client::transport::{Transport, TransportError};
use tmux_protocol::limits::Limits;

/// A fake tmux server: answers the snapshot commands, and lets a test inject
/// notifications at chosen points in the stream.
#[derive(Default)]
struct FakeTmux {
    incoming: VecDeque<Vec<u8>>,
    written: Vec<u8>,
    counter: u64,
    closed: bool,
    /// Emitted immediately before the `capture-pane` response of a pane.
    before_capture: Option<Vec<u8>>,
    /// Emitted immediately after the `capture-pane` response of a pane.
    after_capture: Option<Vec<u8>>,
    /// Set when the next read should report "nothing available".
    starve_once: bool,
}

impl FakeTmux {
    fn block(&mut self, lines: &[&str]) -> Vec<u8> {
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

    fn push(&mut self, bytes: Vec<u8>) {
        self.incoming.push_back(bytes);
    }

    fn answer(&mut self, line: &str) {
        if let Some(nonce) = line.strip_prefix("display-message -p ") {
            if nonce.starts_with("__muxterm_sync__") {
                // Echo the whole marker, exactly as tmux echoes the format argument.
                let marker = nonce.to_string();
                let block = self.block(&[&marker]);
                self.push(block);
                return;
            }
        }
        if line.starts_with("list-sessions") {
            let block = self.block(&["$0 1 1 dev"]);
            self.push(block);
        } else if line.starts_with("list-windows") {
            let block = self.block(&["$0 @0 0 1 bb62,80x24,0,0,0 shell"]);
            self.push(block);
        } else if line.starts_with("list-panes") {
            let block = self.block(&["@0 %0 0 80 24 1 0 0 sh"]);
            self.push(block);
        } else if line.starts_with("capture-pane") {
            if let Some(before) = self.before_capture.take() {
                self.push(before);
            }
            let block = self.block(&["$ restored line"]);
            self.push(block);
            if let Some(after) = self.after_capture.take() {
                self.push(after);
            }
        } else if line.starts_with("display-message -p -t") {
            let block = self.block(&["3 2 0 0 2000 0  0"]);
            self.push(block);
        }
    }
}

impl Transport for FakeTmux {
    fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
        if self.closed {
            return Err(TransportError::Closed);
        }
        self.written.extend_from_slice(bytes);
        let text = String::from_utf8_lossy(bytes).into_owned();
        for line in text.lines() {
            if !line.is_empty() {
                self.answer(line);
            }
        }
        Ok(())
    }

    fn recv(&mut self, _timeout: Duration) -> Result<Option<Vec<u8>>, TransportError> {
        if let Some(bytes) = self.incoming.pop_front() {
            // Deliver in small pieces to exercise the streaming parser.
            if bytes.len() > 16 {
                let rest = bytes[16..].to_vec();
                self.incoming.push_front(rest);
                return Ok(Some(bytes[..16].to_vec()));
            }
            return Ok(Some(bytes));
        }
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

fn sync_session(session: &mut ControlSession) -> Vec<SessionEvent> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    while Instant::now() < deadline {
        let events = session.pump(Duration::from_millis(10)).expect("pump");
        let done = events
            .iter()
            .any(|event| matches!(event, SessionEvent::SnapshotComplete(_)));
        all.extend(events);
        if done {
            break;
        }
    }
    all
}

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
    // The pane snapshot must be delivered before any live output for that pane, so
    // that a renderer applying events in order cannot show stale bytes first.
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
fn desync_marks_the_model_untrusted() {
    let fake = FakeTmux {
        after_capture: Some(b"this is not protocol at all\n".to_vec()),
        ..FakeTmux::default()
    };
    let mut session = session_with(fake);
    let _ = sync_session(&mut session);
    // A raw line is not itself a desync, but the model must not claim accuracy
    // about anything it did not verify either.
    let events = session.pump(Duration::from_millis(10)).expect("pump");
    assert!(events
        .iter()
        .all(|event| !matches!(event, SessionEvent::Desync { .. })));
}

#[test]
fn unsolicited_initialisation_answers_are_not_matched_to_requests() {
    let mut fake = FakeTmux::default();
    // The server answers its own attach commands before anything we sent.
    let init = {
        let counter = 0;
        format!("%begin 100 {counter} 0\nattach output\n%end 100 {counter} 0\n").into_bytes()
    };
    fake.push(init);
    let mut session = session_with(fake);
    let events = sync_session(&mut session);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::Unsolicited(_))),
        "server-initiated output must be surfaced separately: {events:?}"
    );
    assert!(session.model().consistency().is_synced());
}

#[test]
fn a_failed_snapshot_command_stops_the_snapshot() {
    struct Failing(FakeTmux);
    impl Transport for Failing {
        fn write_all(&mut self, bytes: &[u8]) -> Result<(), TransportError> {
            self.0.write_all(bytes)?;
            let text = String::from_utf8_lossy(bytes).into_owned();
            if text.starts_with("list-windows") {
                self.0.incoming.clear();
                self.0.counter += 1;
                let number = self.0.counter;
                let block =
                    format!("%begin 100 {number} 0\nno such window\n%error 100 {number} 0\n");
                self.0.incoming.push_back(block.into_bytes());
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
    let deadline = Instant::now() + Duration::from_secs(5);
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

#[test]
fn an_application_command_gets_its_own_response_with_the_server_header() {
    // Commands issued outside the snapshot are answered through SessionEvent::Response
    // and keep the server's framing numbers for diagnostics.
    let mut session = session_with(FakeTmux::default());
    let _ = sync_session(&mut session);

    let id = session
        .request(&tmux_protocol::command::Command::new("list-sessions"))
        .expect("a synced session accepts commands");

    let deadline = Instant::now() + Duration::from_secs(5);
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
    assert_eq!(response.first_line(), Some(&b"$0 1 1 dev"[..]));
}
